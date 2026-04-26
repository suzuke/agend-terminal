# Sprint 19 Cleanup Report

> **Scope freeze**: Sprint 19 umbrella-final decision (post-challenge round, 2026-04-26).
> Each track appends its section. This file aggregates LOW-archived moves (committed in track PRs) and MEDIUM/HIGH candidates (operator/dev-lead review required, no auto-merge).

## Track 2: Docs / decisions archive (dev-impl-2)

### Archive moves (LOW — committed in this PR)

- `docs/UX-TASK-BOARD-REDESIGN.md` → `docs/archived/sprint-17/UX-TASK-BOARD-REDESIGN.md`
  - Reason: Sprint 17 PR-AV design proposal, design-only (0 production code). Implementation shipped in Sprint 18 PR-AY (#197 merged).
  - Cross-ref count: 0.
  - Broken-link check: pass — `rg -F "UX-TASK-BOARD-REDESIGN.md" .` returns 0 hits after move.
- `docs/INSTANCE-MONITOR-DESIGN.md` → `docs/archived/sprint-17/INSTANCE-MONITOR-DESIGN.md`
  - Reason: Sprint 17 PR-AX design proposal, design-only (0 production code). Implementation shipped in Sprint 18 PR-AZ (#196 merged).
  - Cross-ref count: 0.
  - Broken-link check: pass — `rg -F "INSTANCE-MONITOR-DESIGN.md" .` returns 0 hits after move.

Convention note: `docs/archived/` (past-tense) folder preserved per existing repo convention (17 pre-existing files use the flat layout). New archives this sprint use `docs/archived/sprint-XX/` subfolder where XX = the doc's authoring sprint. The flat-vs-subfolder mismatch is left for a future sweep — out of Track 2 scope.

### MEDIUM candidates — docs (no auto-merge, operator review)

| File | Reason | Recommendation |
|---|---|---|
| `docs/REVIEWER-CONTRACT-v0.1.md` | Superseded by Reviewer Contract v1.1 (§3 of `docs/FLEET-DEV-PROTOCOL-v1.md`, explicit "Extends Reviewer Contract v0.1 with structured tooling"). 0 filename cross-refs. Sprint attribution ambiguous — doc dates itself "Wave 2 (2026-04-22)" which predates sprint numbering. | Archive once operator picks target folder (e.g. `docs/archived/pre-sprint/` or `docs/archived/sprint-1/`). |
| `docs/CHANNEL-AUDIT-COMMS.md` | Sprint 12 PR-AC audit doc (read-only, 0 production code). 0 filename cross-refs but conceptually feeds `PLAN-channel-abstraction.md` and `PLAN-channel-ux-layer.md` roadmaps (both still active per high cross-ref count: 12 / 13). | Keep until channel abstraction roadmap closes; then archive to `docs/archived/sprint-12/`. |
| `docs/CHANNEL-AUDIT-GIT.md` | Sprint 12 PR-AB audit doc, references active strategic decision `d-20260425080249077056-10` (git-server-agnostic direction). 0 filename cross-refs. | Keep — strategic direction still active. Re-evaluate after git-server abstraction lands. |
| `docs/DESIGN-topic-delete-ux.md` | Status: "draft — discussion paused, awaiting user clarification" (2026-04-21). 0 cross-refs. Not shipped, not formally abandoned. | Operator decide: continue, abandon (archive), or formal close. |
| `docs/design-team-layout-auto-tab.md` | Status: "DRAFT v2 ... pending user approval" (2026-04-23). 0 cross-refs. Implementation likely shipped (team auto-grouping behavior is observable) but design doc has no "shipped" stamp. | Operator verify which sprint shipped it → archive `sprint-N/`; else keep as draft. |
| `docs/DESIGN-waiting-on-heartbeat.md` | Track 1 design doc; `set_waiting_on` MCP tool + heartbeat is shipped and referenced in protocol §7. 0 filename cross-refs. Sprint number not stamped in doc body. | Likely safe to archive — operator confirm sprint number and target folder. |
| `docs/HANDOVER-weak-model-instructions.md` | Handover doc on weak-model instruction-following failures. 0 cross-refs. Whether this concern is still active is unclear without operator context. | Operator review: ongoing concern → keep; resolved → archive. |
| `docs/PLAN-agend-core-extract.md` | Status: "Not started 2026-04-20". 0 filename cross-refs (the 5 mentions found are self-references / `architecture.md` aside). 1 week stale, but doc explicitly states blockers (third consumer for `BackgroundServices`). | Keep — actively planned with documented blockers. Re-evaluate if plan abandoned. |
| `docs/PLAN-multica-learnings.md` | Status: "Planning (not started)" 2026-04-17. 0 cross-refs. 10 days no movement. | Operator decide: still planning or abandoned. |

(`docs/FOLLOWUP-merge-idle-ready.md` was scanned but has 1 active cross-ref from `PLAN-state-replay-fixture-expansion.md` and status "Open — not scheduled" → keep, not a candidate.)

### MEDIUM candidates — decisions (daemon storage, not git-tracked)

Decisions live in daemon storage, not in `docs/`. `git mv` does not apply; archive must go through the daemon API (`update_decision` with archive flag, or equivalent). Listed here for operator/dev-lead action.

**Archive candidates** (sprint closed + scope freeze stale):

- `d-20260423155041294464-0` "Sprint 3 scope (修訂版): PTY header 強化取代 MCP notifications" — supersedes `d-20260423152242924999-5`. Sprint 3 closed in `d-20260423164009940077-1`. Pure historical pivot record.
- `d-20260425022429149810-5` "Sprint 9 PR-T re-scope (supersede previous Sprint 9 plan)" — supersedes `d-20260424225500735728-3`. Sprint 9 closed in `d-20260425030616653991-6`; the re-scoped `interrupt` MCP tool merged as PR #159.
- `d-20260426083743565610-1` "Sprint 14 PR-AO scope change 撤回" — supersedes `d-20260426083614177798-0`. Sprint 14 PR-AO #184 has shipped; retraction directive no longer load-bearing.

**Keep active (do not archive)**:

- `d-20260426164715192899-1` "Sprint 18.5 HOTFIX scope — B Hybrid" — supersedes `d-20260426164111610820-0`. **In-flight** (dev-impl-1 active on Sprint 18.5 HOTFIX per Sprint 19 umbrella decision). Active scope freeze.

### HIGH risk — out of scope (not touched)

Per dispatch out-of-scope clause:

- `docs/FLEET-DEV-PROTOCOL-v1.md` — protocol doc.
- `docs/USAGE.md`, `docs/CLI.md`, `docs/MCP-TOOLS.md`, `docs/architecture.md` — user-facing setup/reference (high cross-ref count: 5–9 each).
- `MEMORY.md` (operator personal memory) — operator-only edits.

### Out of Track 2 scope

- Track 5 (cleanup-discovered bug) — none discovered during this Track 2 sweep.
- Reorganizing the existing 17 flat files in `docs/archived/` into sprint subfolders — convention mismatch noted but deferred.
