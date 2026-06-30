# Deep Audit 2026-06-29 — Final Regression Audit Report

Repository: **agend-terminal** · Branch: `deep-audit-fixes` (17 commits on `063c35f2`) · ~286k LOC Rust.
Verdict: **PASS** — full automated suite green, no regressions, lints clean, two review-pass defects
found and fixed.

---

## Headline numbers

| Metric | Value |
|---|---|
| **Total Features** | **~250** user-facing surfaces inventoried (88 consolidated rows: CLI 34, MCP 77, TUI 52, daemon 55, config/channels/backends 60+, core domain 34) across 6 subsystems |
| **Total User Stories** | **166** — 107 happy-path (prior `…-feature-tracker.xlsx`, PR #2507) + 59 adversarial (edge/invalid/interrupted/async/cancellation) added this pass |
| **Total Issues Found** | **21** confirmed (this deep audit) + 7 from the prior pass (ERR-001..007). Plus **~18 refuted** (code defends correctly) and **4 uncertain** — all documented |
| **Issues Fixed** | **17** (root-cause, each with tests + regression) + **2 review-pass completions** (012 cross-process base, 013 lock-GC) |
| **Remaining Known Issues** | **4 deferred** (002b, 006, 009, 014) + the pre-existing `docs/KNOWN_ISSUES.md` set (#1339 operator mode, etc.) |

## Issues Fixed (17)
001 SSRF/forge-token gate · 002 force_release + delete_instance per-caller ACL · 003 config safety-gate
operator-only · 004 interpreter env-injection deny-list · 005 metadata size cap · 007 crash-arm panic
isolation · 008 Stage-2 notify backoff · 010 cron DST fall-back storm (repro-verified 180→2 fires) ·
011 task-id pid disambiguation · 012 runtime-config atomic + cross-process-correct write · 013 skills
stage per-digest lock · 015 parent-dir fsync durability · 016 close_tab focus mis-route · 017
scroll_offset clamp · 018/019/020 docs drift + dead env var.

Every fix: one commit (DCO sign-off + Co-Authored-By), root-cause not symptom, behaviour preserved
except where the issue required a change. **12+ automated tests added.**

## Remaining Known Issues (4 deferred, with rationale)
| ID | Why deferred |
|----|--------------|
| **006** event-bus blocking the tick thread | Full fix = offload subscriber/notify delivery to a bounded worker queue — a large daemon-core refactor that warrants a dedicated, well-tested pass, not a tail-of-session rush. Interim mitigation available: an explicit `reqwest` timeout on the notify `Bot` bounds the worst-case stall. |
| **009** CI rerun-to-green dedup swallow | Needs a per-workflow / per-sha-aggregate redesign of `select_runs_to_notify` (scalar dedup model today); high regression risk on the load-bearing CI-notification path. |
| **002b** repo-merge per-caller ACL | A PR has no instance-ownership model and merge is already CI-green-gated; a per-caller ACL needs a clearer ownership definition first. |
| **014** cross-board dependency claim race | Narrow multi-board-only edge; soft-gate violation (not data corruption); fix cost (lock foreign boards) exceeds the impact. |

## Regression Results
**Authoritative run is against REAL git** — this shell runs inside the AgEnD daemon, whose `agend-git`
PATH shim intercepts the git operations the worktree tests perform (it is a fleet-worktree governor,
not a test harness; CI runs without it). Under the shim, 91 git/worktree tests fail spuriously; under
real git they pass. **None of the 17 fixes touch worktree-management code**, confirming the shim
failures are an environment artifact, not a regression.

| Surface | Env | Result |
|---|---|---|
| Binary unit suite (`cargo test --bin`) | **real git** | **4593 passed / 0 failed** / 13 ignored |
| Library unit tests (`--lib`) | real git | 25 passed / 0 failed |
| Integration `cli_smoke` (end-to-end CLI) | real git | 8 passed / 0 failed |
| Invariants (atomic_write, core_mutex, block_on_guard, anti_pattern, cargo_include) | real git | all passed / 0 failed |
| **clippy** (`unwrap_used = deny`) | real git | **0 warnings / 0 errors** |
| Adversarial diff review (independent agent over the 16-commit diff) | — | 2 defects found → **both fixed + re-tested green** |
| Binary suite under the AgEnD `agend-git` shim | shim | 4502 / 91 — all 91 are shim-blocked git/worktree tests (env artifact) |

**No regressions.** Every previously-passing test still passes under the correct (real-git) environment;
every fix carries a test that fails without it. The full breakdown is in the `Regression 2026-06-29`
sheet of the canonical spreadsheet.

## Repository Health Assessment
**Healthy / production-trajectory for a pre-alpha.** Signals:
- 4600+ automated tests, all green; strict lints (`unwrap_used` deny) clean; rich invariant tests that
  encode design rules (lock ordering, atomic writes, spawn-site rationale).
- The audit **refuted ~18** plausible failure modes — the codebase already defends against most edge
  cases (poisoned-lock avoidance via parking_lot, append-log crash-safety, fail-closed schema guards).
- Strong test density in the load-bearing subsystems (task board, inbox, daemon supervisor/restart,
  ci_watch). Weaker spots (below) are honest gaps, not rot.

## Remaining Technical Debt
- **One systemic authorization root (#1339):** the operator gate is fully permissive in the default
  `Active` mode, so per-tool ACLs are the only defence. This pass added the highest-value ACLs (002/003)
  but the proper fix is the deferred operator-mode policy freeze.
- **006 / 009** are real reliability debt on the daemon tick thread and CI-notification path (deferred above).
- **Test-vs-shim coupling:** the worktree tests can only run under real git; running the suite inside a
  daemon-managed shell needs `PATH=/usr/bin:$PATH`. Worth a one-line note in the test docs.
- **Lighter-tested areas** (from inventory): live channel behaviour (Telegram/Discord — mostly trait
  scaffold), quickstart network flows, Windows paths, performance/load. `team delete` partial-cascade and
  fleet merge-classification exhaustiveness remain **Uncertain**, not confirmed.

## Recommendations
1. **Merge `deep-audit-fixes`** — 17 verified fixes, all green, clippy clean. (Open a PR; CI runs real git.)
2. **Schedule 006 as a dedicated session** — the worker-queue refactor; or ship the interim `reqwest`
   timeout now to bound the worst case.
3. **Open a design issue for the #1339 operator-mode root** — fold 002b and the remaining authorization
   surface into it rather than per-tool patches.
4. **Tackle 009** with a per-workflow dedup redesign + a multi-workflow rerun regression test (the band
   that's currently untested).
5. **Add a test-runner note**: run the suite with real git outside the daemon shell (or add a CI-parity
   wrapper) so the worktree tests aren't shim-blocked locally.
6. Land regression tests for the **untested edge bands** named above (DST transition hours done ✅;
   multi-workflow CI and concurrent same-profile boot still open).

---
*Generated 2026-06-29. Per-issue status: `DEEP-AUDIT-2026-06-29-ISSUES.md` (✅ markers) and the
`Audit2 Tracker` + `Regression 2026-06-29` sheets in the canonical spreadsheet.*
