# PLAN: Sprint 46 — M5 self-route bug structural fix + reserved-name warning

**Date:** 2026-05-02
**Status:** PLAN-first 4-perspective synthesis complete; awaiting operator §13 GO before any IMPL dispatch.
**Branch:** `lead/sprint46-plan-m5-structural`
**Origin:** general m-20260502102244341875-219 dispatch on operator GO ("46 先開" + "general by design 不是 bug、ea377a 抓錯 / 但加 warning OK").
**Process:** PLAN-first 4-perspective synthesis (STRUCTURAL · PRIOR-ART · MINIMAL-DELTA · COST-BENEFIT) per §13.
**Scope decision:** pending operator GO; this PLAN doc qualifies for Tier-1 docs-only self-merge per §3.5.5 LOW after operator approval.

---

## 0. KISS gate (§0) + operator constraint

- **What real problem does this solve?** Sprint 44 M5 hotfix (PR #383) improved the error message when dispatching `request_kind=task target_instance=dev` (collides with `templates.dev` whose orchestrator is `lead`), but did NOT solve the underlying name shadowing. Lead has been carrying a workaround (plain send + `[delegate_task]` header) since Sprint 45 PR-1 dispatch, repeatedly across 14+ Sprint 45 dispatches. The structural fix retires the workaround for good and reduces lead cognitive overhead.
- **Would deletion break anyone?** No — all options are net-add behavior; existing fleets without name collision see no change.
- **Operator constraint:** ≥80% code-level enforcement (Sprint 44 standing rule). Tier-2 dual review for dispatch-core changes. Reserved-name warning is opt-in friction, not a hard reject.

---

## 1. Verified current state (lead minimal-delta vantage)

### 1.1 fleet.yaml current state (collision exists in production)
```yaml
instances:
  dev:        # kiro-cli worker (real instance)
  lead:       # claude orchestrator
  reviewer:   # codex reviewer
  general:    # operator-proxy assistant
templates:
  dev:        # team template — same name as instance!
    orchestrator: lead
    instances:
      lead: ...
      impl-1: ...
      impl-2: ...
      reviewer: ...
```
The collision `templates.dev` ↔ `instances.dev` is the trigger.

### 1.2 Code path
- `src/mcp/handlers/comms.rs::handle_delegate_task` line 159 — calls `resolve_team_orchestrator` first, no instance-existence check.
- `src/teams.rs::resolve_team_orchestrator` line 330 — looks up `templates.X.orchestrator`; returns `Some(orchestrator)` even when `X` is also an instance name.
- `src/api/handlers/messaging.rs:29` — fires the self-route check that produces the user-visible error.
- `src/tasks.rs::instance_exists` line 51 — already-existing helper that loads fleet.yaml and checks `instances.contains_key(name)`. Permissive on parse error and missing fleet.

### 1.3 Sprint 44 M5 status
Sprint 44 PR #383 added a pre-check at `handle_delegate_task` post-resolve:
```rust
if *sender == target && raw_target != target {
    return json!({"error": "task target X resolved to sender Y (team orchestrator loop)..."});
}
```
This pre-check is defense-in-depth — improves error message UX, but does not change the resolution semantics. With Sprint 46 Option A in place, this pre-check still catches edge cases (e.g., a future template→orchestrator chain that loops back to sender even without a same-name instance).

---

## 2. Three perspectives (synthesis)

### 2.1 lead — MINIMAL-DELTA

Option A (instance-first lookup) is the minimum-churn fix:
- Reuse `tasks::instance_exists` helper at `handle_delegate_task` line ~159
- 15 LOC change: early-return-with-direct-target when instance exists; only fall back to `resolve_team_orchestrator` when no instance match
- Zero schema change; zero migration; zero breaking change for existing fleets without collision
- Sprint 44 M5 pre-check stays in place as defense-in-depth

### 2.2 dev (kiro) STRUCTURAL — m-225

| Option | Code site | LOC | Breaking risk | Test footprint | Backward compat |
|---|---|---|---|---|---|
| **A instance-first** | `comms.rs::handle_delegate_task` line 160 | ~15 | LOW (no collision = identical behavior) | 3 (collision-shadow / no-collision / team-only) | Zero migration |
| B namespace separation | `fleet.rs::FleetConfig::load` startup validation | ~30 | HIGH (current operator fleet rejected) | 2 (reject collision / pass no-collision) | Migration deprecation period required |
| C target_kind field | `comms.rs` + `tools.rs` + `messaging.rs` API schema | ~40 | HIGH (all callers update) | 5+ (each kind + fallback + schema) | Optional field with current-behavior fallback (still hits bug) |

dev recommends **Option A** + reserved-name warning bundled into the same PR (~10 LOC at `agent::validate_name`, same cognitive surface).

### 2.3 reviewer (codex) PRIOR-ART — m-224

Five routing systems analyzed:
1. **k8s DNS (Service vs Pod)** — namespace-encoded names + SRV records prevent shadowing. Lesson: encode resource kind in name.
2. **Git refs (branch/tag/SHA)** — deterministic disambiguation order + explicit escape hatch (`heads/main`). Lesson: deterministic fallback + explicit override.
3. **Bash builtin vs PATH** — local/builtin first, PATH fallback. Lesson: prefer "more local/core" target — directly applicable as instance-first.
4. **npm scopes (`@scope/pkg`)** — prefix-namespace prevents collision syntactically. Lesson: namespace separation is cleanest long-term.
5. **Rust 2018 module path** — local item shadows extern crate; explicit `::crate` for external. Lesson: local-first default + explicit syntax.

Recommendation: **A short-term + B long-term** — A solves the immediate bug, B (`team:` / `instance:` namespace prefix) is a cleaner Sprint 47 evolution.

### 2.4 reviewer (codex) COST-BENEFIT — m-227

Final ranking: **A > B > C**.

| | LOC | Breaking | Migration | UX | Risk | Long-term hygiene |
|---|---|---|---|---|---|---|
| A | 15 | none | none | continuous | lowest | medium (collision still possible, predictable) |
| B | 30 | high | required | clearer | medium | high |
| C | 40 | high | required+API churn | API friction | highest | high |

Reserved-name warning bundle decision: **agreed** — same naming surface, warn-only, low coupling. Bundling cost < split-PR coordination overhead.

Tier classification:
- Option A (dispatch core) = **Tier-2 dual**
- Warning + A in same PR = **Tier-2** (high-risk surface dominates)
- Warning split (if separated) = Tier-1 single

---

## 3. BLOCKING items

None. All four perspectives converge on Option A as the recommended fix. The single divergence (PRIOR-ART suggests B as long-term Sprint 47 evolution) is captured as a forward-looking question for operator (§8 #5).

---

## 4. Phase rollout

| PR | Scope | LOC est | Tier | Trigger |
|---|---|---|---|---|
| **PR-1** | Option A instance-first lookup at `handle_delegate_task` + reserved-name warning at `agent::validate_name` | ~15 (M5 fix) + ~10 (warning) + ~30 (tests) = ~55 LOC total | **Tier-2 dual** (touches dispatch core) | Dispatchable post operator §8 GO |

Total **~55 LOC, 1 PR**, est. **30-45 min** for dev (mechanical fix mirroring Sprint 45 PR-1/5/7/8 single-PR success pattern).

`code:protocol` ratio: ~50 code / ~5 protocol (one-line note in §3.6 if any) = **~91/9** ✓ ≥80/20 floor.

### Reviewer config (Sprint 45 retrospective default)
- codex single PRIMARY
- lead cross-vantage check (independent grep + cargo test both modes + cargo fmt --check)
- §3.5.4 dual-review satisfied via codex + lead

---

## 5. §3.5 / §3.6 application

- **§3.5.10 production-path-coupled**: PR-1 ships with integration test that runs through real `handle_delegate_task` dispatch with collision fixture (templates.test-dev orchestrator=lead-test + instances.test-dev) — verifies instance-first path.
- **§3.5.11 test-first**: 3 RED+GREEN test pairs land first; refactor to GREEN.
- **§3.5.13 verdict externalization**: applies to PR-1's own review.

---

## 6. Cumulative risks

| Risk | Mitigation |
|---|---|
| Existing callers depend on team-resolution-as-default behavior | grep audit at lead pre-dispatch verify; test that team-only target (no instance) still resolves to orchestrator. |
| Sprint 44 M5 pre-check redundant with new instance-first path | Keep pre-check as defense-in-depth — it catches future template→orchestrator chains that loop back to sender for reasons other than name shadowing. |
| Reserved-name warning false-positive | Warn-only, never blocks. Operator can ignore. List vetted: `general`, `lead`, `dev`, `reviewer`, `system:auto_close`, `system:overdue_sweep`, `system:task_sweep`. |
| Sprint 47+ Option B migration complexity | Captured as §8 #5 forward question. PR-1 doesn't preclude B — A is purely additive. |

---

## 7. Out of scope (this Sprint)

- Sprint 47 Option B namespace prefix (`team:`/`instance:`) — separate sprint with migration path.
- Option C `target_kind` API field — not pursued.
- Renaming current fleet.yaml `templates.dev` (would resolve collision but doesn't fix the underlying resolver behavior).
- Migrating Sprint 44 M5 pre-check (kept as defense-in-depth).
- 16-pattern reviewer prompt catalog reshape.
- IMPL of any phase. PLAN-only.
- Daemon binary auto-rebuild post-merge (Sprint 44.5+ candidate).

---

## 8. §13 candidate questions for operator

1. **Option A approval** — instance-first lookup at `handle_delegate_task` line 160 as the immediate behavior change baseline?
2. **CLI resolution hint** — when collision detected (instance + template same name), should CLI return an explicit resolution message (e.g., "resolved to instance:dev, ignoring templates.dev orchestrator route")? Or silent prefer-instance is fine?
3. **Reserved-name list** — should the warning cover `general` / `lead` / `dev` / `reviewer` only, or also `system:auto_close` / `system:overdue_sweep` / `system:task_sweep` (Sprint 45 PR-1 SYSTEM_IDENTITIES allow-list)?
4. **Warning escalation path** — keep warn-only forever, or upgrade to hard reject in a future sprint after ecosystem warms up?
5. **Sprint 47 Option B planning** — green-light scoping `team:` / `instance:` namespace prefix as Sprint 47 follow-up, or accept Option A as terminal solution?
6. **Regression test** — add explicit regression test fixture for the canonical collision case (templates.dev + instances.dev + orchestrator=lead) to prevent re-introduction?
7. **Reserved-name bundle in PR-1** — confirm acceptable to bundle (~10 LOC warning) into Option A PR (Tier-2), vs split into a separate Tier-1 single PR?
8. **PLAN-doc self-merge path** — Tier-1 docs-only post operator GO (§3.5.5 LOW exception)?
9. **Backward compat verification** — should PR-1 add an explicit unit test that team-only targets (no matching instance) still resolve to orchestrator correctly (i.e., A doesn't break the team-template feature)?

---

## 9. Status

**PLAN authored 2026-05-02T10:26 UTC** with all 4 perspectives folded.

- STRUCTURAL (dev kiro) — m-20260502102530224602-225 ✓
- PRIOR-ART (reviewer codex) — m-20260502102502464692-224 ✓
- MINIMAL-DELTA (lead claude) — §1 + §2.1 ✓
- COST-BENEFIT (reviewer codex cross-vantage) — m-20260502102614857311-227 ✓

Forwarding to general for operator §8 GO answer. On GO:
1. Tier-1 docs-only self-merge of this PLAN (§3.5.5 LOW exception).
2. PR-1 IMPL dispatch to dev with M5-fix-A + reserved-name-warning success criteria.
3. Sprint 46 closeout report after PR-1 merge.
