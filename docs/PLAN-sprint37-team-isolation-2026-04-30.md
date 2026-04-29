# PLAN: Sprint 37 — Daemon-side team isolation enforcement

**Date**: 2026-04-30
**Basis**: main HEAD `745d95a` (post Sprint 34 + side-fix #337 in flight)
**Operator brief**: general m-99 at 2026-04-29T16:11Z — three rules, zero escape hatch. Replace trust-based cross-team workarounds with daemon-mechanical enforcement.
**Team**: lead2 (orchestrator, minimal-delta + cost-benefit) · dev2 / kiro-cli (structural) · reviewer2 / codex (prior-art)

---

## 0. Scope (frozen by operator)

The rule:

```
fn check_routing(sender, target):
    if sender == target: allow                          // self
    if target == "general" || sender == "general": allow // fleet bus
    if same_team(sender, target): allow                 // intra-team
    else: BLOCKED
```

**No** `cross_team_auth_ref`, **no** orchestrator detection, **no** `parent_id` auth inheritance, **no** escape hatch.

Cross-team flows must go through `general` (operator's TUI / Telegram bridge) or via `create_instance(team=own_team)` to add workforce. Worker-to-other-team-worker direct messaging is intentionally forbidden.

The 4-perspective challenge round answers **how minimal · prior art · cost-benefit boundaries**, NOT whether-to.

---

## 1. The fundamental framing question (resolved up-front)

**Q**: Is Sprint 37 rebuilding what Sprint 28 (PR #285) deleted?

**A** (per reviewer2 prior-art m-104, attestation language):

> Sprint 37 is **NOT** rebuilding Sprint 28 RBAC. Sprint 28 removed per-operation outbound capability checks (`channel/auth.rs::evaluate_outbound_capability` etc.), defending against a malicious / prompt-injected agent attempting unauthorised operations on already-authenticated channels. Sprint 37 adds sender-target team-boundary routing checks on `send` (identity / topology gating). No per-op capability matrix, no `ChannelOpKind` policy churn, no revival of `evaluate_outbound_capability` semantics.

The two are orthogonal dimensions:

| Dimension | Sprint 28 (deleted) | Sprint 37 (added) |
|---|---|---|
| Question | WHAT operation may an authenticated agent perform | WHO may route to WHOM |
| Defends against | malicious / prompt-injected agent abusing operations | accidental cross-team dispatch + trust-based discipline drift |
| Mechanism | per-operation capability matrix, multi-arm `OutboundCapabilityDecision` | one-time membership lookup (Option equality on Team) |
| State | per-instance + per-op + global `Mutex<HashSet>` warn dedup | stateless query (`teams.rs::find_team_for`) |
| Existing comparable pattern in codebase | none after PR #285 | YES — `decisions.rs:45`, `tasks.rs:93/97/466/469/499/648/773`, `comms.rs:174/546` (continuity argument) |

**Continuity argument** (from reviewer2): team-membership-based gating is **already established codebase pattern** for decisions and tasks (orchestrator-of, resolve-team-orchestrator). Sprint 37 extends that continuity to the `send` boundary. The ONLY way Sprint 37 drifts into RBAC-resurrection territory is if implementation strays into per-operation matrix or `ChannelOpKind`-style policy. The PLAN keeps it narrowly as sender-target routing.

---

## 2. The 3 rules — implementation shape (lead2 minimal-delta)

### 2.1 Where it lives

`src/api/handlers/messaging.rs::handle_send` (the daemon-side choke point dev2 confirmed in §3.1 below). Insert AFTER target-existence check and BEFORE message construction.

```rust
// Rule 1: self-send was already rejected at line 30 above (pre-existing behaviour).
//   from == target → already returns "cannot send to self" error.
//   No change needed.

// Rule 2: general bus
let is_general_bus = from == "general" || target == "general";

// Rule 3: same-team (Option<Team> equality semantics — NO `_default_` string)
let from_team = crate::teams::find_team_for(ctx.home, from);
let target_team = crate::teams::find_team_for(ctx.home, target);
let same_team = match (&from_team, &target_team) {
    (Some(a), Some(b)) => a.name == b.name,
    (None, None) => true,        // both in implicit no-team pool
    _ => false,                  // boundary mismatch → block
};

if !is_general_bus && !same_team {
    // Audit + reject
    crate::event_log::log(
        ctx.home, "send_cross_team_blocked", from,
        &format!(
            "target={target}, sender_team={:?}, target_team={:?}",
            from_team.as_ref().map(|t| &t.name),
            target_team.as_ref().map(|t| &t.name),
        ),
    );
    return json!({
        "ok": false,
        "error": format!(
            "cross-team send blocked: '{from}' (team={:?}) → '{target}' (team={:?}). \
             Route via general, or use create_instance(team=...) to grow your team.",
            from_team.as_ref().map(|t| &t.name),
            target_team.as_ref().map(|t| &t.name),
        )
    });
}
if is_general_bus && !same_team {
    // Allowed-via-general — log for full trace per operator brief
    crate::event_log::log(
        ctx.home, "send_cross_team_allowed_general", from,
        &format!("target={target}"),
    );
}
```

### 2.2 Estimated diff

- `src/api/handlers/messaging.rs::handle_send`: +35-40 LOC (gate logic + audit calls + error message)
- `src/api/handlers/messaging.rs::tests`: +60-80 LOC (5 fixture cases per operator brief + a no-team / `agent_ops::send_to` fallback regression guard)
- Total: ~95-120 LOC. Operator estimate of 60-100 was tight; the audit-log calls + clear error message put us at the upper end. Still Tier-2-appropriate; no other file touched.

### 2.3 What is explicitly NOT changed

- `react` / `reply` / `schedule` / other MCP tools — not in this sprint per operator scope freeze.
- No cryptographic signature / `auth_ref` / per-op capability matrix.
- No `team` MCP action (create/delete) — already exists, untouched.
- No `_default_` team string in fleet.yaml — see §3.4 / §4 below.

---

## 3. Findings from team perspectives

### 3.1 Structural (dev2, m-111) — confirmed single choke point + 1 bypass

**`handle_send` IS the single daemon-side choke point** for inbox-routed sends. All `method::SEND` callers funnel through it (5 sites traced — comms.rs:91/265/435, api/mod.rs:307, agent_ops.rs:31). MCP `handle_unified_send` → `handle_send_to_instance` → `agent_ops::send_to` → `api::call(SEND)` → `handle_send`. `handle_broadcast` iterates targets via `agent_ops::send_to` → covered automatically per-target.

**Out of scope (correctly bypass)**:
- `app/commands.rs:285` `:send` TUI command → `agent::send_to_registry` → PTY inject. Operator-typed, not agent-to-agent. No inbox.
- `src/channel/telegram.rs:~790` Telegram adapter → `inbox::enqueue` directly. Operator → agent, not agent → agent. Operator override sits above daemon rules.

**Known bypass (acceptable)**:
- `agent_ops::send_to` fallback path (`agent_ops.rs:53-58`): when daemon API is unreachable, falls back to direct `inbox::deliver` — bypasses `handle_send`. dev2 structural read: acceptable for v1 because daemon-down means no agents are running (they are daemon-managed children); only narrow race window during shutdown. Document as known limitation; do not gate on it.

### 3.2 Prior-art (reviewer2, m-104) — RBAC framing distinct, continuity strong, audit sink known

- **RBAC framing**: distinct (per-op capability vs sender-target routing). Sprint 37 is NOT contradiction unless implementation drifts to per-op matrix.
- **`teams.rs` API sufficient**: `find_team_for(home, member) -> Option<Team>` is enough. No new API. No new field on fleet.yaml.
- **Continuity argument**: team-membership-based gating already in `decisions.rs`, `tasks.rs`, `comms.rs:174` (delegate), `comms.rs:546` (broadcast). Sprint 37 extends pattern to `send`.
- **`_default_` team mechanism**: does NOT currently exist. Current no-team behaviour = `team = None` lane (Fleet Peers fallback in instructions). Adopting `_default_` would be a new policy layer and require migration — not necessary if `Option<Team>` equality is used (see §3.4).
- **Audit sink**: `src/event_log.rs::log(home, kind, instance, detail)`. Already daemon-level audit channel with persistence + rotation + fsync. Recommended kind strings: `send_cross_team_blocked`, `send_cross_team_allowed_general`.

### 3.3 Minimal-delta (lead2)

The minimum implementation that satisfies the spec is **6 logical statements** at `handle_send`:

1. `from == target` → already-rejected (pre-existing; no change).
2. `from == "general" || target == "general"` → general bus, allow.
3. Two `find_team_for` calls.
4. `Option<Team>` equality check (`Some/Some name match` OR `None/None`).
5. If `same_team` false AND not general bus → audit log + reject.
6. If general bus AND not same-team → audit log allowed-cross.

That's it. No new struct, no new fleet.yaml field, no new MCP action, no new daemon thread. Operator's "zero escape hatch" framing maps cleanly onto `Option<Team>` equality semantics.

The audit + clear error message inflate LOC to ~95-120 (from a bare ~25 LOC), but the audit is a hard requirement per operator brief (every cross-team attempt logged) and the error message is the OPERATOR-FACING substitute for the deleted escape hatches (callers need to know HOW to legitimately cross teams: via general or by spawning into own team).

### 3.4 Cost-benefit (lead2 — `Option<Team>` equality vs `_default_` string)

**Recommendation: use `Option<Team>` equality semantics. Do NOT introduce a `_default_` team string.**

Comparison:

| Approach | Code | fleet.yaml impact | Migration | Future maintenance |
|---|---|---|---|---|
| `Option<Team>` equality | natural (3 LOC match arm) | none | **none** — existing fleets unaffected | mechanical — `find_team_for` already supports None |
| `_default_` team string | special-case fallback `find_team_for` to return `Some("_default_")` | **new reserved name** in fleet.yaml semantics | every existing instance needs a check / explicit team field | risk of accidental teamless drift |

Operator's KISS philosophy + Sprint 28's deletion of paranoid layers strongly favours the `Option<Team>` approach. The implicit-pool semantics naturally handles backward-compat (existing teamless fleets work without change) and new teamed fleets get strict isolation.

**Edge case** (None/Some block): an agent spawned without team config CANNOT send to a teamed agent (and vice versa). This is intentional per "no escape hatch" — if you want an agent to talk to a teamed agent, give it a team. The error message guides the operator to `create_instance(team=...)`.

---

## 4. §13 decisions surfaced for operator

### Q1 — `_default_` team semantics

**Recommendation**: **do NOT introduce `_default_`**. Use `Option<Team>` equality where:
- `None == None` → allow (both in implicit no-team pool)
- `Some(A) == Some(A)` → allow
- `Some(A) ≠ Some(B)` → block
- `Some(A) ≠ None` → block (boundary mismatch)

dev2 structural + reviewer2 prior-art + lead2 cost-benefit all converge here.

### Q2 — fleet.yaml migration (force every instance to have team field)

**Recommendation**: **NO migration**. With `Option<Team>` equality, existing fleets work unchanged. Teamless instances simply talk to other teamless instances + general. To opt into team isolation, the operator adds a team field — incremental, no big-bang migration.

### Q3 — `general` identity recognition

**Recommendation**: **literal name match**, no fleet.yaml flag. Reasoning:
- The string `"general"` is already a special-cased identity throughout the codebase (orchestration / fleet-bus role).
- A fleet.yaml flag would add config surface for zero gain — operators can't run a parallel fleet-bus under a different name without the daemon's hardcoded affordances anyway.
- KISS: literal check `from == "general" || target == "general"` is one line, no schema dependency.

If operators ever rename or run a non-`general` fleet-bus, that's a separate sprint to introduce a `kind: fleet_bus` field. Not in scope here.

### Q4 — Sprint number

**Recommendation**: **Sprint 37** (operator's chosen number; no clash observed in `git log` or task board).

### Q5 — Backward compat (grandfather existing fleet behaviour)

**Recommendation**: **YES, grandfather automatically via `Option<Team>` semantics**. No special grandfather code needed. Existing teamless instances communicate freely (None/None allow); new fleets opting into teams get strict isolation; mixed fleets get block-at-boundary (which is the intended discipline).

### Q6 — `agent_ops::send_to` daemon-down fallback bypass

**(NEW §13 question raised by dev2 structural review.)**

**Recommendation**: **acceptable v1 limitation, document explicitly**. When the daemon is down, no agents are running (they are daemon-managed children); the bypass is a narrow race window during shutdown. Adding the gate at the fallback path requires duplicating `find_team_for` logic (which itself reads disk via `teams.rs`) into the no-daemon code path — not impossible, but inflates LOC and adds a second-source-of-truth risk. Defer to Sprint 38 if a real incident shows the bypass matters.

### Q7 — Tier classification

**Recommendation**: **Tier-2 dual reviewer** (operator's brief explicitly stated this; daemon routing boundary change). reviewer2 PRIMARY + reviewer (dev team) cross-vantage per operator authorisation in m-99 ("Cross-team auth：Sprint 35 PR #333 同 lineup. reviewer2 (codex, dev2 team) PRIMARY + 借 reviewer (codex, dev team) cross-vantage. Per operator authorization on 2026-04-30").

The recursive irony: this very dispatch (cross-team borrowing reviewer for cross-vantage) is the pattern Sprint 37 formalises. **Operator authorisation IS the override that sits above the daemon rule** — once Sprint 37 lands, this kind of cross-team review will need explicit operator approval (which it already has for this sprint), not implicit lead2 trust-based dispatch. Going forward, lead2 will route cross-team review requests via `general` rather than direct `send`, and operator's approval will be the audit-log line authorising the bypass.

---

## 5. Acceptance criteria (per operator brief §3.5.10 mandatory tests)

Every test target uses the persistence-replay shape (real disk, fresh `home`, no in-memory mocks):

| # | Test | Pre-condition | Action | Assertion |
|---|---|---|---|---|
| 1 | `send_same_team_allowed` | sender + target both in team `dev2` | `send` from sender to target | `ok: true`, message in target inbox, NO `send_cross_team_blocked` audit |
| 2 | `send_cross_team_blocked` | sender in `dev2`, target in `dev` | `send` from sender to target | `ok: false`, error mentions both team names, NO message in target inbox, audit `send_cross_team_blocked` written |
| 3 | `send_to_general_allowed_from_any_team` | sender in `dev2`, target = `general` | `send` from sender to general | `ok: true`, audit `send_cross_team_allowed_general` written |
| 4 | `send_from_general_to_any_team_allowed` | sender = `general`, target in `dev` | `send` from general to target | `ok: true`, audit `send_cross_team_allowed_general` written |
| 5 | `send_self_already_blocked` | sender = target = `dev2` (just sanity-check pre-existing rule still fires before team gate) | `send` self | `ok: false`, error "cannot send to self" — NOT the team gate's error |
| 6 | `send_no_team_to_no_team_allowed` (BACKWARD COMPAT) | sender + target both teamless | `send` between them | `ok: true`, no team-gate audit |
| 7 | `send_team_to_no_team_blocked` (BOUNDARY CASE) | sender in team, target teamless | `send` from sender to target | `ok: false`, audit `send_cross_team_blocked` |
| 8 | `send_no_team_to_team_blocked` (BOUNDARY CASE, INVERSE) | sender teamless, target in team | `send` from sender to target | `ok: false`, audit `send_cross_team_blocked` |

---

## 6. §3.5.10 wire-format external fixture (mandatory, Tier-2 strict)

The 8 fixtures above all hit the daemon-side `handle_send` directly through real `api::call(method::SEND)` — exercising the wire path end-to-end (no in-memory mock of the gate). Each test creates a real fleet.yaml on disk, real teams.json, and asserts the actual `event_log` JSONL line was appended.

Per protocol §3.5.10 wire-format scope (file is in `src/api/`), this qualifies as **wire-format invariant test** for the new boundary (the gate's input/output surface is the wire of the API).

## 7. §3.5.11 test-first

Two-commit RED-then-GREEN per protocol:

- **RED commit**: 8 fixture tests + helper scaffolding. All cross-team / boundary-mismatch tests fail (current `handle_send` lets them through).
- **GREEN commit**: gate logic + audit calls. All 8 tests pass. Pre-existing self-send test still passes.

## 8. Implementation wave

dev2 single track, Tier-2 dual reviewer:

- dev2 (kiro-cli) IMPL: ~95-120 LOC + 8 fixtures
- reviewer2 (codex, dev2 team) PRIMARY Tier-2: §3.5.10 disposition + §3.5.11 attestation + scope-conformance check
- reviewer (codex, dev team) cross-vantage Tier-2: independent fresh-eyes per Sprint 35 PR #333 lineup pattern (operator authorisation on 2026-04-30 covers this borrow); focus on:
  - bypass-completeness (any path that escapes `handle_send`)
  - error-message wording usability for blocked sender
  - audit-log payload sufficiency for incident replay

Both VERIFIED + CI green required for self-merge per §3.5.4.

## 9. Process notes

- **Worktree**: `/Users/suzuke/.agend-terminal/workspace/lead2/repo` on `plan/sprint37-team-isolation-2026-04-30` off `745d95a`
- **Decision (PLAN scope freeze)**: `d-...will-post`
- **Dispatches**:
  - reviewer2 PRIOR-ART — dispatched 2026-04-29T16:15Z, reported 16:16Z (~1 min wall — research had clear targets)
  - dev2 STRUCTURAL — dispatched 2026-04-29T16:18Z, reported 16:22Z (~4 min wall — completed in parallel with side-fix #337 review-and-CI)
- **PR path**: §3.5.5-extended LOW docs-only single-reviewer self-merge (operator-authorised path used in Sprint 33 / 34 PLANs).
- **Side-fix dependency**: PR #337 (Sprint 34 PR-5 follow-up team-spawn metadata cleanup) is in flight; Sprint 37 PLAN merges independently.

## 10. Self-awareness on the recursive irony

This sprint exists because lead2's dispatch pattern of borrowing dev-team `reviewer` for Sprint 33 PR-3 + Sprint 34 PR-5 cross-vantage Tier-2 reviews (a trust-based workaround) prompted operator to formalise the boundary in code. Acknowledged in §4 Q7: post-Sprint-37, lead2 routes cross-team review requests via `general` rather than direct `send`, and the operator's explicit approval becomes the audit-log line authorising each bypass. This very PLAN dispatch (cross-team review of operator-authored Sprint 36/37 PLAN) is the LAST trust-based cross-team-review action lead2 takes before Sprint 37 enforcement lands; from that point, the cross-team-via-general ritual becomes mechanical.
