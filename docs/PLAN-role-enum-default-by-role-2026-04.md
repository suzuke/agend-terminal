# PLAN — Role enum + default-by-role refactor (Sprint 28+ companion)

**Date**: 2026-04-28
**Author**: dev-reviewer-2 per operator m-41 #2 GO directive ("一勞永逸 KISS pattern")
**Scope**: docs-only plan; impl deferred to Sprint 29+ post-Sprint-28 worktree opt-out land
**Companion to**: `docs/PLAN-team-worktree-branch.md` §2.5 (this plan formalises what §2.5 deferred as "too magical to auto-detect")
**Status**: PROPOSAL — awaiting 4-perspective challenge round + operator GO

## §0 Problem

`InstanceConfig` accumulates per-role boolean opt-out flags:

- `receive_fleet_updates: Option<bool>` — Sprint 22-class for chat-proxy `general` instance
- `worktree: Option<bool>` — Sprint 28 Gap #1 for orchestrator/reviewer instances
- (future projected) `needs_branch`, `is_silent`, `commits_back`, `receives_dispatch` — each gap surfaces another flag

Each boolean must be set per-instance in `fleet.yaml`. Forgetting one defaults to the conservative-broad-cast behaviour appropriate for `Implementer` instances but inappropriate for orchestrators/reviewers/proxies. Result: stale `agend/dev-lead` branches, fleet broadcast spam in user-chat proxies, cwd-locked reviewers — all of which the existing flags fix individually after the fact.

The KISS question: **N independent booleans vs. 1 role enum with default-by-role + escape-hatch overrides — which is simpler?**

This plan argues for the enum.

## §1 Role enum proposal

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    /// Coordinates work; never commits. Stable cwd, no worktree, receives fleet updates.
    Orchestrator,
    /// Writes code; commits to its own branch under a worktree. Receives fleet updates.
    Implementer,
    /// Reviews PRs; floats between branches via `checkout_repo`. No own worktree. Receives fleet updates.
    Reviewer,
    /// Read-only docs/lint/analysis. No worktree, no commits, silent (doesn't receive fleet updates).
    Utility,
    /// User-chat proxy (e.g. `general`). No worktree, no commits, silent.
    Proxy,
}
```

5 variants — covers every current fleet member without forcing a 6th.

## §2 Default-by-role table

| `role_kind` | `worktree` | `commits` | `git_branch` | `receive_fleet_updates` | `fleet_dispatch_target` |
|---|---|---|---|---|---|
| `Orchestrator` | **false** | false | — | true | true (receives task delegations) |
| `Implementer` | **true** | true | `agend/<name>` | true | true |
| `Reviewer` | **false** | false | — (uses `checkout_repo`) | true | true |
| `Utility` | **false** | optional | optional | **false** | optional |
| `Proxy` | **false** | false | — | **false** | false (chat in / chat out only) |

The bold cells encode what currently requires per-instance booleans. Defaults apply unless explicitly overridden.

## §3 Sprint 28 Gap #1 collapse

`docs/PLAN-team-worktree-branch.md` §2 ships `worktree: Option<bool>` short-term. This plan absorbs that into the role default:

| Phase | Schema |
|---|---|
| **Sprint 28 (interim)** | `worktree: Option<bool>` per-instance — ships as planned |
| **Sprint 29+ (this plan)** | `role_kind: RoleKind` per-instance + `worktree` becomes opt-out **override** for the rare special case |

Migration: every `worktree: false` value in fleet.yaml maps to a role assignment; remaining `worktree: false` entries become explicit overrides. Implementers default to worktree=true automatically (no field needed).

## §4 Migration plan

### §4.1 Data layer

`InstanceConfig` gains `role_kind: Option<RoleKind>`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceConfig {
    pub role: Option<String>,           // free-string description (existing)
    pub role_kind: Option<RoleKind>,    // NEW — enum default selector
    // existing fields stay; their defaults now driven by role_kind when present
    ...
}
```

`Option<RoleKind>` (not enum-with-Default-variant) because:
- absent value = "legacy entry, treat per pre-amendment behaviour" — preserves backward-compat for fleet.yaml configs that haven't migrated yet
- forces explicit role declaration on new entries (`role_kind: implementer`) — no silent defaulting that hides intent

### §4.2 Default resolution order

When `resolve_instance` reads `worktree` (or any role-defaulted field):

1. If instance has explicit value → use it
2. Else if `role_kind` is set → use the role's default (per §2 table)
3. Else fall back to current behaviour (Sprint 28 `Option<bool>` semantics)

### §4.3 fleet.yaml migration mapping

Recommended one-time migration (run as a `agend-terminal fleet migrate-roles` subcommand or manual edit):

| Existing instance | Inferred `role_kind` |
|---|---|
| `dev-lead` | `orchestrator` |
| `dev-impl-1`, `dev-impl-2`, `kiro-cli-*` (when used as impl) | `implementer` |
| `dev-reviewer`, `dev-reviewer-2` | `reviewer` |
| `general` | `proxy` |
| docs/lint analysts | `utility` |

Migration is **opt-in** — operators choose when to add `role_kind: <kind>` to existing entries. Until added, the entry uses Sprint 28 per-instance booleans (backward-compat path).

### §4.4 Per-instance override (back-compat)

Explicit fields override the role default:

```yaml
instances:
  dev-impl-readonly:
    role_kind: implementer
    worktree: false   # this impl is read-only despite role_kind=implementer
```

Override is a single boolean line — same as Sprint 28 baseline. The role enum reduces *needed* fields, doesn't *forbid* them.

### §4.5 Validation

`resolve_instance` warns when role default + explicit override conflict semantically (e.g. `role_kind: orchestrator` + `git_branch: agend/dev-lead`) so operator notices stale config.

## §5 KISS justification

### §5.1 N booleans vs 1 enum

| Aspect | N booleans | Enum + default-by-role |
|---|---|---|
| Lines per instance in fleet.yaml | up to N (one per opt-out) | 1 (`role_kind: <kind>`) — overrides only when special |
| Forgetting a flag | silent wrong default (bug) | role-based default catches the common case |
| Adding a new role-correlated field | edit every instance + add a new flag everywhere | extend the role table once |
| Onboarding readability | "what does `worktree: false, receive_fleet_updates: false, ...` mean?" | `role_kind: proxy` self-documents |

### §5.2 The "too magical" rejection in plan §2.5 — addressed

Plan §2.5 rejected auto-detection from `role: Option<String>` (free-string description) because string parsing is brittle. This plan does NOT auto-detect — it asks the operator to *declare* the role with a typed enum. Declaration is the opposite of magic; it's the most explicit form possible.

### §5.3 When NOT to add the enum

If the fleet stays at ≤2 distinct roles (e.g., one orchestrator + one impl, no reviewer/proxy/utility distinctions), the enum is over-engineering. Current fleet has 5+ distinct roles, so the threshold is well past.

## §6 Sequencing + non-goals

- **Sprint 28 ships**: `worktree: Option<bool>` per `PLAN-team-worktree-branch.md` §2 (interim). This plan does NOT block Sprint 28.
- **Sprint 29 over-engineering audit ships first** (per dev-reviewer-2 m-41 sequencing recommendation). RBAC removal may simplify what role-based access means in practice, informing this plan's enum semantics.
- **Sprint 29+ (this plan impl)**: `role_kind` enum + default resolver ships AFTER audit lands.
- **Non-goal — auto-migration**: operators run the migration mapping manually OR via a one-shot subcommand. No silent rewriting of fleet.yaml.
- **Non-goal — strict role enforcement**: the role declaration drives defaults; it does NOT prevent an instance from using arbitrary commands. RBAC enforcement is out of scope (and being audited for removal in Sprint 29).

## §7 Open questions for 4-perspective challenge round

1. Does the 5-variant enum cover all current + projected fleet members, or is a 6th variant needed (e.g. `Cron` for scheduled-routine instances)?
2. Should `kiro-cli-*` instances default to `Implementer` (matches current usage) or be a separate variant?
3. Is `Option<RoleKind>` the right sentinel, or should `RoleKind::Implementer` be the implicit default for backward-compat?
4. Migration timing — opt-in (this plan) vs forced flag-day (operator runs `migrate-roles` once)?
5. Per-instance override semantics when override fights role default — warn vs error vs silent?

## §8 Cross-references

- `docs/PLAN-team-worktree-branch.md` §2.5 — this plan formalises the rejected auto-detection
- `docs/audit-over-engineering-2026-04-28.md` #1 RBAC removal — independent of role declaration; declaration ≠ enforcement
- `docs/FLEET-DEV-PROTOCOL-v1.md` §3.6.9 git auto-cleanup — Orchestrator/Reviewer roles' default `worktree=false` directly reduces the cleanup workload §3.6.9 enforces
- Sprint 26 PR #271 amendment batch + Sprint 27 PR #277 amendment batch — precedent pattern for operator-directive-driven docs PRs landing as Path A LOW docs-only

## §9 Self-qualification

This plan is docs-only, qualifies under §3.5.5 LOW docs-only single-reviewer (Path A — dev-reviewer review). LOC budget ~200 LOC; condition #2 ≤50 LOC threshold disqualifies — this is a substantive plan doc, NOT a §3.5.5 qualifying amendment. Reviewer attestation needed: ".plan doc only, no rule introduction".

§3.5.10/11/12/13/14/15 N/A (no protocol-layer src/ change).

§3.6 dogfood — push and immediately continue. Per task t-20260428063656232759-10 success criteria.
