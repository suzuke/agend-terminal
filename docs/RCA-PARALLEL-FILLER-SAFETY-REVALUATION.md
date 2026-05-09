# RCA — Parallel filler safety re-evaluation post bind_self rebase mode

**Sprint 60 W3 PR-2 — Path B doc, Sprint 60 closeout PR.**
Re-evaluates the sequential-default dispatch rule (Sprint 59 W2)
now that `bind_self(rebase_mode=true)` (Sprint 60 W1 PR-1 #578)
ships atomic stale-lease recovery.

---

## 1. Incident recap + mitigation

Sprint 59 W1 PR-2: PR-1 + PR-2 both extended `enforce_send_invariants`
in parallel; PR-2's `gh pr merge` returned merge conflicts. The
remediation needed a clean worktree for rebase, but `release_worktree`
+ `bind_self` returned `lease_failed` because the on-disk dir lingered
from the prior cycle. Escape hatch: `AGEND_GIT_BYPASS=1` for
`git worktree add`. Post-hoc disclosure landed in §6 closeout; ledger
adjusted to "35 ledger-clean post-policy + 1 scoped-bypass-incident-
with-disclosure" per Q1=(b).

Mitigation (Sprint 59 W2): sequential-default dispatch rule per
`feedback_parallel_pr_conflict_resolution.md` — filler-during-reviewer-
wait deferred until in-flight PR closes.

---

## 2. P0-1 mitigation analysis (#578 rebase_mode)

**What it fixes:** the exact `lease_failed` failure mode. Single
atomic MCP call: `bind_self(branch, rebase_mode=true)` invokes
`force_release::rebase_clean_self` before the lease — clears stale
on-disk dir + binding, then binds cleanly. 5 helper unit tests + 2
end-to-end `handle_bind_self` tests in #578 cover the contract.
The engineering-layer BYPASS root cause is closed.

**What it does NOT fix:** GitHub-side merge conflicts at `gh pr merge`
time. Recovery still requires manual rebase + force-push to the
self-owned PR branch (allowed per
`feedback_parallel_pr_conflict_resolution.md` — gate is on BYPASS,
not on force-push to self).

**Cross-agent isolation preserved:** `dispatch_auto_bind_lease` still
rejects branches leased by another agent. `rebase_mode=true` only
releases self's stale state; cannot steal a peer's branch. Q2=(C)
intact.

---

## 3. Three options

- **A — Re-enable parallel filler.** Throughput unlock, rebase_mode is
  the safety net. Cost: merge-conflict resolution remains manual; no
  multi-agent lease-thrash test coverage.
- **B — Sequential default + opt-in parallel via dispatch flag.** Safe
  baseline preserved; lead invokes parallel only when filler PR's file
  surface is disjoint from the in-flight PR per dispatch's predicted
  file-touches list. Cost: opt-in cognitive overhead.
- **C — Keep sequential-default permanently.** Simplest; misses the
  throughput opportunity that #578 partially unlocks.

---

## 4. Recommendation: **Option B**

Rationale:

1. **Safe default.** Sequential remains the dispatch baseline — no
   surprise parallelism for typical waves; Sprint 59-60 clean-cycle
   pattern continues.
2. **Throughput available where it matters.** Lead opts in when the
   filler PR's predicted file surface is disjoint from the in-flight
   PR. The throughput gain is real for naturally-disjoint waves.
3. **Recovery is BYPASS-free.** If a parallel dispatch produces a
   merge conflict: `kind=query` to lead → authorized rebase →
   `bind_self(rebase_mode=true)` for the rebase worktree → no BYPASS
   needed. Force-push to self-owned branch already allowed.
4. **Cross-agent isolation unchanged.** rebase_mode only affects self.

The dispatch flag's exact schema is out of scope. Minimum viable
opt-in: lead's dispatch text includes "parallel-feasible vs PR-X
(file surface disjoint per audit)" and dev treats that as
authorization to claim before the in-flight PR closes. A formal
schema is a Sprint 61 candidate.

---

## 5. Smoke test deferred

Component 2 (Path A smoke) not shipped. Reason: meaningful "parallel
filler safety" testing requires multi-agent coordination test
infrastructure that doesn't exist yet. `bind_self(rebase_mode=true)`
already has 5 unit + 2 end-to-end tests in #578 covering the
single-agent atomic recovery contract. The genuinely new failure
surface — *concurrent* multi-agent lease churn — needs a multi-process
test harness, itself a Sprint 61+ candidate. Proceeding without smoke
matches the dispatch's "if smoke can't reveal issues, defer" guard.

---

**Summary.** rebase_mode (#578) closes the engineering-layer BYPASS
escape hatch. Sequential-default rule should remain (Option B
baseline) but parallel filler becomes opt-in for explicit cases
where file surfaces are disjoint. Recovery from parallel-induced
merge conflicts is now BYPASS-free. Cross-agent isolation unchanged.
Multi-agent coordination test harness is a Sprint 61 candidate.
