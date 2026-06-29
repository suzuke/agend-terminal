# Deep Audit 2026-06-29 — Summary

Independent second-pass repository audit of **agend-terminal** (`main` @ `4bf67128`, ~286k LOC Rust,
516 files). Goal: understand every user-facing feature, infer intended behaviour, and surface
**new** defects beyond the prior pass — **without changing any code** (discovery + documentation only).

## What this builds on
- The prior audit (**PR #2507**, by `fugu-0acdd8`, same day) produced
  `docs/audit/agend-terminal-user-story-feature-tracker.xlsx`: 107 happy-path user stories, 20 manual
  test runs, and **7 errors** (ERR-001..007; 5 fixed, 2 won't-fix). Those are resolved and **not**
  re-reported here.
- This pass goes deeper at the **code** level (handlers, services, state files, concurrency) and is
  **adversarially verified**: every candidate defect was confirmed or refuted against quoted source.

## Deliverables (this folder)
| File | Phase | Contents |
|------|-------|----------|
| `DEEP-AUDIT-2026-06-29-FEATURE-INVENTORY.md` | 1 | Canonical inventory of ~250 features across 6 subsystems → expected/test/finding |
| `DEEP-AUDIT-2026-06-29-USER-STORIES.md` | 2 | Adversarial user stories (edge/invalid/interrupted/async/…) |
| `DEEP-AUDIT-2026-06-29-ISSUES.md` | 4 | **20 verified issue drafts** grouped by root cause + Refuted + Uncertain appendices |
| `DEEP-AUDIT-2026-06-29-PRIORITIZATION.md` | 5 | Severity buckets + 6-wave fix order |
| `DEEP-AUDIT-2026-06-29-SUMMARY.md` | — | This file |

> **No GitHub issues filed.** Per operator decision (2026-06-29) Phase 4 stops at local drafts;
> filing awaits review + a chosen target repo.

## Methodology
1. **Inventory (Phase 1):** 6 parallel read-only agents mapped CLI / MCP / TUI / daemon /
   config-channels-backends / core-domain against the `docs/FEATURE-*.md` spec → ~250 features +
   ~100 red-flag candidates.
2. **Discovery + verification (Phase 3):** 5 parallel adversarial agents each took a cluster
   (auth/validation · concurrency/persistence · TUI state-machine · daemon reliability · doc-drift),
   read the **actual** code, and classified each candidate **REAL / UNCERTAIN / REFUTED** with quoted
   file:line evidence. Findings cross-checked against `docs/KNOWN_ISSUES.md` and open GitHub issues.
3. **Synthesis (Phases 4-5):** confirmed findings grouped by root cause, severity-rated, ordered.

## Findings at a glance
**20 confirmed issue drafts** (counting grouped docs as one): **3 High**, ~12 Medium, ~5 Low — plus
an Uncertain shortlist. Highest-value:

| # | Finding | Sev | One-liner |
|---|---------|-----|-----------|
| AUDIT2-001 | SSRF + token exfil via `ci_provider_url` | High | forge token sent to any agent-supplied host, reachable by least-privileged role |
| AUDIT2-011 | Task-ID collision at `send(kind=task)` | High | silent task loss; regression guard is falsely green |
| AUDIT2-009 | CI rerun-to-green swallowed (≥2 workflows) | High | silent broken reviewer handoff ~50% of the time |
| AUDIT2-006/07/08 | Blocking I/O on the tick thread | Med | telegram stall / crash-arm panic can wedge or kill the daemon |
| AUDIT2-002/03/04 | Missing per-tool ACL / env deny-list gaps | Med | destructive + exfil tools reachable by any agent in default mode |
| AUDIT2-012/13 | Non-atomic runtime-config / skills-stage race | Med | config corruption flips safety gates; fleet-boot skill loss |
| AUDIT2-010 | Cron DST mis/double-fire | Med | untested transition-hour band |
| AUDIT2-016/17 | TUI focus mis-route / blank pane | Med | wrong-tab input; blank scrollback after alt-screen/zoom |
| AUDIT2-018/19/20 | Stale `USAGE.md` + dead env/param | Low–Med | documents commands & a binary that don't exist |

**Two systemic root causes** worth a design issue above the point fixes:
- *Default-`Active` operator gate is fully permissive* → per-tool ACLs are the only authorization,
  and several destructive/sensitive tools have none (AUDIT2-002/003/004, impersonation caveat).
- *Notification I/O runs inline on the single daemon tick thread* → one stalled subscriber blocks
  the fleet (AUDIT2-006/007/008).

**Recurring danger traits:** *silent failure* (succeed-but-lose: AUDIT2-009, 011) and *untested edge
bands* (DST transition hours, multi-workflow CI, concurrent same-profile boot).

## Rigor note — what was checked and is FINE
The audit **refuted** ~18 plausible-looking failure modes where the code defends correctly (role
fail-open, decision author spoof, worktree double-release, inbox stuck-forever, task-log compaction
crash-safety, focus_id panic, image-paste off-by-one, palette overrun, restart successor race, …).
The full list is in `ISSUES.md § Refuted` — recorded so future audits don't waste effort re-raising
them.

## Resolution status — implementation pass (branch `deep-audit-fixes`)
**17 of 20 issues fixed** (root-cause, with tests + regression, one commit each); **4 deferred** with
rationale. Each fix is recorded in the `Audit2 Tracker` sheet (commit hash) and the per-issue ✅ markers
in `ISSUES.md`.

| Status | Issues |
|--------|--------|
| ✅ Fixed | 001 (SSRF/token gate), 002 (force_release + delete_instance ACL), 003 (config safety-gate lock), 004 (env-injection deny-list), 005 (metadata cap), 007 (crash-arm panic isolation), 008 (Stage-2 notify backoff), 010 (cron DST storm — repro-verified 180→2), 011 (task-id pid), 012 (runtime-config atomic+lock), 013 (skills stage lock), 015 (parent-dir fsync), 016 (close_tab focus), 017 (scroll clamp), 018/019/020 (docs/dead-env) |
| ⏸ Deferred | **006** (event-bus → bounded worker-queue: large daemon-core refactor, warrants a dedicated well-tested pass rather than a tail-of-session rush — interim mitigation available: an explicit `reqwest` timeout on the notify `Bot`); **009** (CI rerun-to-green dedup: needs a per-workflow/per-sha-aggregate redesign of `select_runs_to_notify`, high regression risk on load-bearing CI); **002b** (repo merge ACL: no instance-ownership model, already CI-green-gated); **014** (cross-board dep claim race: narrow multi-board edge, low impact) |

All 17 fixes share the worktree and compile cleanly together; each was validated against the relevant
test suite. Severities here are the **adjudicated** ones (operator review), which differ from the
original drafts (e.g. 011 High→Low after confirming the collision is single-process-unreachable today).

## Coverage & limitations (honest scope)
- **Covered breadth-first:** all 6 subsystems inventoried; the highest-risk red flags in each were
  verified to source. This is **not** an exhaustive line-by-line review of 286k LOC.
- **Lighter coverage:** channels (Telegram/Discord live behaviour — mostly trait scaffold, untested),
  quickstart network flows, Windows-specific paths, and performance/load behaviour were inventoried
  but not deeply fuzzed. The `team delete` partial-cascade and fleet merge-classification
  exhaustiveness are flagged **Uncertain**, not confirmed.
- **No runtime testing:** findings are from static reading + the prior xlsx's manual runs; they are
  high-confidence (evidence-quoted) but not all were reproduced on a live daemon. Recommended next
  step is a regression test landing **with** each fix, in the exact edge band named.

## Suggested next actions
1. Review `ISSUES.md`; decide which drafts to file and to which repo (`suzuke/agend-terminal` vs the
   `justdoit` fork) — then I can open them.
2. Start Wave 1 (AUDIT2-001, 011, 007): small diffs, high impact.
3. Open two design issues for the systemic roots above.
