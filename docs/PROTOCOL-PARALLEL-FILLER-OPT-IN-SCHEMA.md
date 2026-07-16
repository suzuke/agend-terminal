# Parallel filler opt-in — formal schema

> **Status: SUPERSEDED — historical Sprint 61 dispatch convention.** The literal
> `parallel-filler:` marker is not part of the live MCP schema and does not
> authorize worktree, rebase, force-push, or merge exceptions. Current behavior
> is governed by [`FLEET-DEV-PROTOCOL.md`](FLEET-DEV-PROTOCOL.md) §10, §12.1,
> §12.4, and §12.6: each task uses a daemon-managed branch/worktree, pipeline
> depth is at most two, and PRs in one wave merge sequentially with rebase,
> fresh CI, and refreshed verdict evidence between merges. The old
> `force_release_worktree` / `restart_daemon` tool names and the ad-hoc
> force-push recovery recipe below are retired. The remainder is preserved only
> as the original incident/design record.

**Sprint 61 W2 PR-1 — process protocol.** Closes Sprint 60 #583 §5
deferral by replacing the MVP "parallel-feasible vs PR-X" dispatch-text
semantic with a formal opt-in schema. Sequential-default remains the
baseline per #583 Option B; this doc specifies the explicit opt-in
contract for cases where lead/general dispatches in parallel.

---

## 1. Default: sequential

The dispatch baseline is unchanged: filler-during-reviewer-wait is
deferred until the in-flight PR closes. Without an explicit opt-in
flag, dev's worktree only ever holds one branch; no two PRs ever race
for the same files at merge time.

Reference: Sprint 59 W2 establishment per
`feedback_parallel_pr_conflict_resolution.md` after the Sprint 59 W1
PR-2 BYPASS incident.

---

## 2. Opt-in flag (formal)

A dispatch authorizes parallel filler if and only if its body
contains the literal marker line:

```
parallel-filler: opt-in (vs PR-<number>; file surface: <path1>, <path2>, ...)
```

- `vs PR-<number>` — the in-flight PR this filler runs alongside. Must
  be present.
- `file surface: <paths>` — explicit list of files this dispatch
  expects to touch. Operator-auditable disjointness check against the
  in-flight PR's predicted file touches list.

Examples of correctly formed opt-in lines:

```
parallel-filler: opt-in (vs PR-587; file surface: scripts/check_loc_overrun.sh, .github/workflows/loc-overrun-check.yml)
parallel-filler: opt-in (vs PR-585; file surface: docs/PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md)
```

Without this exact marker, the dispatch is sequential-default and dev
must wait for the in-flight PR to close before claiming the new task.

---

## 3. Pre-conditions for opt-in (lead's responsibility)

Lead must verify all four before issuing an opt-in dispatch. Each
condition has a documented mitigation if violated:

| # | Pre-condition | Violation mitigation |
|---|---|---|
| 1 | File surface disjoint from in-flight PR's predicted touches | Issue sequential dispatch instead |
| 2 | In-flight PR has stable `<!-- LOC-EST: X-Y -->` marker | Wait for marker convention compliance per #582 §5.1 |
| 3 | Both PRs target same base branch (typically `main`) | Cross-base parallel dispatches not supported in this protocol |
| 4 | No third in-flight PR shares either's file surface | Defer to next sequential window |

---

## 4. Safety net infrastructure (engineering layer)

The opt-in protocol relies on three Sprint 60 W1 + W1-recovery PRs as
safety nets. Recovery from a parallel-induced failure mode never
requires `AGEND_GIT_BYPASS=1`:

- **#578 `bind_self(rebase_mode=true)`** — atomic stale-lease recovery
  if dev's worktree state lingers after the in-flight PR closes.
- **#571 `force_release_worktree`** — operator-callable cleanup of
  on-disk stale worktree dirs when binding registry has cleared but
  the dir lingers.
- **#580 `restart_daemon` MCP tool** — programmatic daemon restart if
  in-flight MCP calls require fresh state (operator-not-at-computer
  no longer a SPOF).

If a parallel dispatch produces a `gh pr merge` GraphQL conflict, the
recovery path stays per `feedback_parallel_pr_conflict_resolution.md`:
`kind=query` to lead → authorized rebase → `bind_self(rebase_mode=true)`
for the rebase worktree → force-push to self-owned branch (allowed;
gate is on BYPASS, not on force-push to self).

---

## 5. Cohesion-accept override

If the file-surface disjointness pre-condition (§3 #1) cannot be
satisfied but lead/general judges the parallel dispatch worth the
merge-conflict cost (e.g. throughput-critical short window),
reviewer-mediated override is available:

- Lead issues the dispatch with the opt-in marker AND an explicit
  `parallel-filler: cohesion-accept` second line documenting the
  reason and the conflict-resolution plan.
- Reviewer adjudicates the override at PR review time per #582 §5.4
  cohesion-accept option (a) precedent.
- The merge-conflict resolution flow stays the same (per §4 above).

---

## 6. Out of scope (Sprint 62+ candidates)

- **Multi-agent test harness** — automated smoke test exercising
  concurrent `bind_self` from disjoint branches under realistic
  filesystem contention. Per Sprint 60 #583 §5 deferral; the
  single-agent atomic-recovery contract is well-tested by #578's 5
  unit + 2 end-to-end tests.
- **Tooling enforcement** — automated parser of dispatch bodies
  validating the opt-in marker + pre-condition fields. The current
  protocol relies on lead's discipline; tooling is a Sprint 62+
  candidate if drift becomes observable.
- **Cross-base parallel dispatches** — opt-in protocol assumes both
  PRs target the same base branch. Cross-base scenarios (e.g.
  release-branch backports landing in parallel) need a separate
  protocol; out of scope for the typical filler case.

---

**Summary.** Sequential-default remains baseline (Sprint 60 #583
Option B). Parallel filler opt-in requires the literal marker line +
4 pre-conditions verified by lead. Recovery from parallel-induced
failure is BYPASS-free via #578 + #571 + #580 safety net. Cohesion-
accept override available for non-disjoint cases per #582 §5.4
precedent. Multi-agent test harness + tooling enforcement deferred
to Sprint 62+.
