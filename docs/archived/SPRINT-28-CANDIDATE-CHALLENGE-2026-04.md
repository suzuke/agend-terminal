# Sprint 28 candidate challenge — 2026-04

Per operator m-226 directive + decision `d-20260428063610856979-0`. Sprint 27 wave fully closed
(PR #277 amendment batch merged `d3e5510` 14:22Z). Operator-authored
`docs/PLAN-team-worktree-branch.md` (`plan/team-worktree-branch` @ `c9f7a35`) requires
fleet 4-perspective self-critique before Sprint 28 dispatch.

## 4 perspectives

- **dev-impl-1 (minimal/skeptical)** — m-19
- **dev-impl-2 (structural)** — m-20
- **dev-reviewer (prior-art)** — m-22
- **dev-reviewer-2 (cost/benefit)** — m-24

All 4 returned proposals within ~30 min of dispatch.

## Convergent baseline

- **Gap #1 Worktree opt-out (`Option<bool>`)** — 4/4 GO Sprint 28
- **Gap #2 Task.branch field** — 3/4 GO same-sprint (impl-1 DEFER convention) — operator m-226 #1
  framing supported by prior-art + cost-benefit; impl-1 disagrees on "zero-risk" claim
- **Gap #3 task_sweep cross-validate** — DEFER per plan §3.4 framing (3/4 supports incident-driven)
- **Long-term role enum** — 3/4 Sprint 29 companion plan (impl-1 NO-GO YAGNI); operator m-226 #2
  framing 「§2.5 標 OR companion follow-up」 supported

## Per-gap synthesis

### Gap #1 — Worktree opt-out (`Option<bool>`)

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | GO (reluctant) | Only because trivial ~10 LOC; if 100+ LOC then NO-GO |
| Structural | GO clean | `Option<bool>` correct short-term + role enum companion long-term |
| Prior-art | GO conditional | 4 anchored conditions (see below) |
| Cost/benefit | GO XS | ~50 LOC + 2 tests, very low risk pure additive default-true |

**Convergence: 4/4 GO Sprint 28.**

**Prior-art conditions (dev-reviewer m-22)**:
1. Plan §2.5 「rejected」 → Sprint 28+ companion plan (per operator feedback #2)
2. §10.4 mandatory worktree integration: `worktree: false` on `role == "implementer" | "reviewer"`
   raises config-load warning OR error (cite `f6a465e` Sprint 18 PR-BA 12+ checkout race events)
3. §3.6.9 atomic-step alignment: plan §2.4 「operator can manually rm」 conflicts with
   PR #277 just-shipped §3.6.9 「self-merge without cleanup is incomplete, not 'deferrable'」
4. §3.5.10 wire-format: fleet.yaml round-trip test + e2e through `bootstrap::resolve_one`
   (NOT unit-level source-grep per PR-A r2 trap class)

### Gap #2 — Task.branch + MCP schema

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | DEFER | Convention sufficient; orchestrator already includes branch in dispatch description; no production incident |
| Structural | GO clean | `Option<String>` + `#[serde(default)]` additive; clean architecture |
| Prior-art | GO same-sprint | Combined PR ~80 LOC stays within §3.5.5 LOW boundary; §3.6.8 takeover gate benefits from `task.branch` |
| Cost/benefit | GO same-sprint | Concur operator m-226 #1, defy plan §4 defer |

**Convergence: 3/4 GO same-sprint (impl-1 DEFER).**

**impl-1 disagreement detail (m-19)**: 「zero-risk」 claim wrong. Adding field is zero-risk
deserialization but `§3.3 items 4-5` (instructions.rs + reviewer protocol changes) alters
agent behavior. If generated AGENTS.md says `git switch -c <branch>` and agent does it
wrong (dirty tree, wrong base), that's a new failure mode. Convention-only has zero new
failure modes.

**3-perspective rebuttal**: 
- Structural: instructions.rs change is one paragraph in AGENTS.md, low risk
- Prior-art: §3.6.8 takeover gate benefits offset incremental risk
- Cost/benefit: dual-PR ship cost is marginal; field-additions with serde defaults

### Gap #3 — task_sweep cross-validate `Closes` marker

| Perspective | Verdict |
|---|---|
| Minimal | DEFER (implicit per Gap #2 DEFER) |
| Structural | implicit GO (mentioned §3.4) |
| Prior-art | DEFER (depends on #2) |
| Cost/benefit | DEFER (opportunistic post-concrete-incident; plan §3.4 framing correct) |

**Convergence: DEFER per plan §3.4 framing — incident-driven follow-up.**

### Long-term role enum + default-by-role

| Perspective | Verdict | Sprint |
|---|---|---|
| Minimal | NO-GO YAGNI | — |
| Structural | Sprint 29 companion | When 3+ features need role-default |
| Prior-art | Sprint 28+ companion plan | §2.5 「rejected」 → companion |
| Cost/benefit | Sprint 29 post-audit | Audit RBAC reshape may reframe role taxonomy |

**Convergence: 3/4 Sprint 29 companion plan (impl-1 NO-GO).**

**Sequencing rationale (cost/benefit m-24)**: Sprint 28 worktree+branch should **precede**
Sprint 29 audit because (a) audit's RBAC simplification affects role enum shape — landing
role enum same-sprint risks rework when audit reshapes role taxonomy; (b) Gap #1
`Option<bool>` is role-orthogonal opt-out — usable regardless of audit outcome.

## Cross-amendment dependency analysis

### §3.6.9 git auto-cleanup (PR #277 just-shipped) ↔ Plan §2.4 risk note

**Conflict identified by 2 perspectives** (prior-art m-22 + cost-benefit m-24):

- Plan §2.4 says: "switching an existing instance from `worktree: true` to `false` leaves an
  orphan `.worktrees/<name>/` that `worktree::prune` may report. Acceptable — operator can
  manually `rm`."
- §3.6.9 (PR #277 d3e5510) says: "self-merge without cleanup is incomplete, not 'deferrable'"

**Resolution options**:
- (A) Plan revision: auto-prune on hot-reload-detected `worktree: true → false` flip
- (B) Plan revision: explicit §3.5.12 deferred-defense post_decision exemption
- (C) §3.6.9 amendment-of-amendment: scope §3.6.9 to merge-cleanup, exempt config-flip-cleanup

Cost/benefit perspective recommends `§3.6.9 reference plan §2.3 once both ship` — complementary
defense framing: §3.6.9 prevents accumulation post-merge; Gap #1 prevents creation pre-dispatch.

**Recommendation**: Sprint 28 implementation PR includes plan §2.4 revision (option A — auto-prune
on hot-reload flip) to align with §3.6.9 atomic-step formulation.

### §10.4 mandatory worktree (Sprint 18 PR-BA `f6a465e`) ↔ Plan §2.2 example

§10.4 (PR-BA `f6a465e` Sprint 18) elevated worktree to MANDATORY for impl/reviewer based on
12+ checkout race events in 1 hour. Plan §2.2 example shows `dev-lead` (orchestrator) opting
out — correct per role-class table. But plan does NOT prevent `dev-impl-1` opting out.

**Recommendation**: Sprint 28 implementation PR adds config-load validation that
`worktree: false` on `role == "implementer" | "reviewer"` raises warning (or error). Operator
m-feedback #2 role enum companion naturally formalizes this.

### §3.5.10 wire-format external-fixture

Sprint 28 PR predicted §3.5.10 class:
- **Wire-format external-fixture**: ✓ applies. fleet.yaml round-trip test (read yaml → struct →
  write yaml → equal) per §3.5.10. fleet.yaml format IS wire format. Backward-compat test:
  omitted field → `None` → behaves as today.
- **Concurrent-state**: NOT applicable (config load-time)
- **Persistence-replay**: NOT applicable (no on-disk state schema change)

§3.5.15 observability e2e applies in spirit: spawn agent with `worktree: false` → assert no
`.worktrees/<name>/` directory created → assert `working_directory` equals input path.
Test must drive `bootstrap::resolve_one` end-to-end (NOT unit-level source-grep).

## Recommended Sprint 28 scope

**Sprint 28 PR-A** (~130 LOC + 5 tests + docs):

1. **Gap #1 worktree opt-out** (~50 LOC):
   - `src/fleet.rs::InstanceConfig`: `pub worktree: Option<bool>` field
   - `src/fleet.rs::ResolvedInstance`: same field, plumbed through `resolve_instance()`
   - `src/bootstrap/agent_resolve.rs::resolve_one`: guard `is_git_repo` block with
     `if resolved.worktree != Some(false)`
   - Validation: `worktree==Some(false) && git_branch.is_some()` → warning
   - **§10.4 integration** (per dev-reviewer m-22 condition #2): warning when
     `worktree==Some(false) && role` matches "implementer"/"reviewer" patterns
   - **§3.6.9 integration** (per dev-reviewer m-22 condition #3): plan §2.4 revised —
     auto-prune on hot-reload-detected `worktree: true → false` flip (option A)

2. **Gap #2 Task.branch field** (~80 LOC):
   - `src/tasks.rs::Task`: `branch: Option<String>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`
   - `src/mcp/tools.rs::task_tools()`: schema entry
   - `src/mcp/handlers/task.rs::handle_task`: wire `args["branch"]`
   - `src/instructions.rs::generate`: AGENTS.md task-claim section paragraph

3. **§3.5.10 wire-format external-fixture**:
   - fleet.yaml round-trip test for new `worktree` field (omitted, true, false cases)
   - e2e test: spawn with `worktree: false` → no `.worktrees/<name>/` directory created

4. **§3.5.11 RED→GREEN strict** per Sprint 25-27 dogfood

5. **§3.5.13 mirror** reviewer verdicts to GH PR comments

6. **§3.6.7 ScheduleWakeup auto-poll** + **§3.6.9 atomic-step cleanup** dev-lead apply on
   self-merge

**Defer to Sprint 28+ companion / Sprint 29 / opportunistic**:
- Gap #3 task_sweep cross-validate (DEFER per plan §3.4 + cost-benefit m-24)
- Sprint 28+ companion plan: role enum + default-by-role design (~40 LOC plan doc, separate PR)
- Sprint 29 over-engineering audit (operator approval pending)

## Operator decisions required

1. **Sprint 28 scope confirmation**: Gap #1 + Gap #2 same-sprint per consensus 3/4 (operator m-226 #1 framing)?
   - **Recommend GO** — 3 perspectives concur with operator framing; impl-1 disagreement on
     "zero-risk" valid concern but addressable via instructions.rs defensive wording.

2. **Sprint 28+ companion plan trigger**: §2.5 「rejected」 → Sprint 28+ companion plan for
   role enum + default-by-role?
   - **Recommend GO** — 3 perspectives concur (impl-1 NO-GO YAGNI); plan §2.5 update
     leaves door open without committing implementation.

3. **§3.6.9 atomic-step alignment** (operator philosophy 「最根本一勞永逸」): plan §2.4 revision
   to auto-prune on hot-reload flip (option A) vs §3.5.12 deferred-defense exemption (option B)
   vs §3.6.9 amendment-of-amendment (option C)?
   - **Recommend Option A** (auto-prune) — preserves §3.6.9 atomic-step rule without exemption;
     Sprint 28 PR-A scope covers the auto-prune logic.

4. **§10.4 validation strictness**: warning vs error when `worktree: false` on impl/reviewer roles?
   - **Recommend warning** — Sprint 28+ companion plan formalizes role enum which makes this
     compile-time enforceable; warning preserves flexibility during transition.

5. **Sprint 29 sequencing**: Sprint 28 worktree+branch precede Sprint 29 over-engineering audit?
   - **Recommend GO sequencing** — audit RBAC reshape may inform role enum companion;
     Sprint 28 `Option<bool>` is role-orthogonal so audit timing doesn't gate it.

## Source verdicts (4 perspectives)

- impl-1 minimal/skeptical m-19 (2026-04-28 14:24)
- impl-2 structural m-20 (2026-04-28 14:24)
- dev-reviewer prior-art m-22 (2026-04-28 14:26)
- dev-reviewer-2 cost/benefit m-24 (2026-04-28 14:28)

## Sprint 27 closure context

PR #277 amendment batch shipped `d3e5510` 14:22Z with 6 entries:
1. §3.5.10 sanctioned-tool decline policy
2. §3.5.14 UX regression prevention
3. §3.5.15 Observability PR e2e requirement
4. §3.6.7 ScheduleWakeup auto-poll
5. §3.6.8 Takeover 4-criteria independent verify
6. §3.6.9 Git auto-cleanup on merge

Sprint 28 PRs ship under §3.5.10 + §3.5.11 + §3.5.12 + §3.5.13 + §3.6 enforcement (all LIVE).

## §3.5.5 self-qualification

This synthesis doc qualifies docs-only PR per §3.5.5 LOW exception → single-reviewer + dev-lead
self-merge. ~280 LOC > 50 LOC §3.5.5 ##### Exemption #2 boundary, but §3.5.5 main rule allows
docs-only LOW per design rationale (operator m-226 NOT direct dispatch impl 沒 GO).
