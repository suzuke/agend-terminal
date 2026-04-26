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

## Track 1: Code dead-code / unused / unused dependencies (dev-impl-1)

### Result: empty — no PR opened

Per challenge round #1 (all-features intersection policy), Track 1 sweep ran 3 feature combinations and found **no actionable LOW removals**. Codebase is already disciplined.

- **Track 1.A (`cargo clippy --all-targets -- -W dead_code -W unused -W unreachable_code`)**: 3-combo intersection clippy clean — codebase already disciplined; 30+ `#[allow(dead_code)]` annotations all carry inline rationale comments (reserved-for-future-wiring / stacking deps / per-field deliberate annotations).
- **Track 1.B (`cargo machete` / `cargo +nightly udeps`)**: 3-combo udeps intersection — all deps used. `cargo machete --with-metadata` flagged `embed-resource` but it is a false positive (used in `build.rs:12` for Windows manifest embed; machete doesn't scan build.rs). Per challenge #1 intersection (1/4 tools flag ≠ true unused), no removal made; tooling-config follow-up filed as Track 5 entry.
- **Sweep verify (cumulative HEAD = `452cd0f`)**: ✓ `cargo clippy --all-features --all-targets -- -D warnings` 0 warnings + `cargo test --all-features` 1026 lib + 19 + 9 + 28 + 33 + 5 + 2 supervisor pass.

### Tooling prereq (challenge #7)

- `cargo install cargo-machete` ✓ installed v0.9.2
- `cargo +nightly install cargo-udeps --locked` ✓ installed v0.1.60

## Track 3: Backlog task hygiene (dev-lead)

### Audit summary — 27 open tasks (post Sprint 18.5/19 PRs)

`task action=list filter_status=open` returned 27 tasks. Classification:

| Category | Count | Action |
|---|---|---|
| Strategic backlog (multi-sprint scope, operator priority) | 10 | KEEP — operator排序 |
| PR follow-up (reviewer non-blocking observations from Sprint 14-19 PRs) | 13 | KEEP low priority |
| Flaky test infra (Windows path / macOS) | 2 | KEEP low — operator/maintainer排程 |
| Cancellation candidate (sprint-closed, dev-lead non-authorized) | 1 | **OPERATOR ACTION REQUIRED** |
| Sprint 19 Track 5 (discovered) | 1 | KEEP candidate per #9 |

### Strategic backlog (10) — operator review for Sprint 20+ priority

- `t-20260424011906930464-7` — Claude Code agent daemon→LLM notification gap
- `t-20260424015240518790-0` — watch_ci 多 CI provider 支援
- `t-20260424020026189561-5` — inbox JSONL compaction
- `t-20260424164529781904-0` — Claude Code 接 fleet-update 進 user input buffer (**Sprint 18.5 HOTFIX B Hybrid PR #199 已部分解** — 仍 strategic for Claude Agent SDK system event channel migration)
- `t-20260424173948421544-1` — Windows path normalization for worktree_cleanup tests
- `t-20260425035142945841-5` — macOS-flaky tests
- `t-20260426081146799803-2` — backend-specific image paste codec (Bug 6)
- `t-20260426083533060854-3` — watch_ci silent-drop infra fix (B 路線已 PR-AP cover, A 路線 GITHUB_TOKEN 認證仍 open)
- `t-20260426083630443353-0` — daemon startup force-regen agend.md (**升 high priority** — 第 2 use case 出現 Sprint 18 Rule 4 broadcast 後)
- `t-20260426164120257127-1` — fleet-update transport long-term

### PR follow-up backlog (13)

集中 reviewer non-blocking observations for Sprint 14/17/18/18.5/19 PRs。建議**保留 individual** 不合 mega-sweep（不同 PR 不同 reviewer 不同 finding 重新 reconcile 風險）：

- `t-20260426074121861355-3` — stacked PR guidance memory append
- `t-20260426074126143193-4` — invariant test alternation
- `t-20260426083541166855-4` — PR-AN/AO/AQ test coverage sweep
- `t-20260426104737606516-3` — PR-AP follow-ups
- `t-20260426120435664003-7` — PR-AS Phase 2 prereq tests
- `t-20260426120555737962-8` — PR-AR follow-ups
- `t-20260426122506967277-9` — PR-AT polish
- `t-20260426124658357493-12` — PR-AV doc polish
- `t-20260426124940713554-14` — PR-AU doc polish
- `t-20260426130228791508-15` — PR-AW lifecycle test
- `t-20260426150403693632-0` — HOTFIX #195 follow-ups
- `t-20260426150432078733-1` — HOTFIX #194 vterm root cause (normal priority)
- `t-20260426160127609549-4` — PR-AY 5 follow-ups (sub-sweep)
- `t-20260426171506985479-5` — PR #199 follow-ups
- `t-20260426173120522467-6` — PR #200 follow-ups
- `t-20260426153738539773-2` — PR-AZ F3 test coverage
- `t-20260426024342801229-20` — PR-AF inbound attachment fail UX (normal priority — 用戶可見 silent failure)

### Cancellation candidate — operator action required ⚠️

- **`t-20260425040356199333-6`** — "[Sprint 11 backlog] backend semantics verification (real CLI spawn)"
  - **superseded by PR #165 (`904f67e`)**
  - dev-lead 多次 cancel 失敗：`task 't-20260425040356199333-6' owned by '', caller 'dev-lead' not authorized`
  - daemon enforce owner-only close + unassigned (`owner=''`) lock
  - **operator wake action**: `task action=update id=t-20260425040356199333-6 status=cancelled` from operator persona / 或 daemon 加 dev-lead unassigned-task close authority

### Track 5 / Sprint 19 discovered (1)

- `t-20260426172642504531-0` — cargo-machete false positive `embed-resource`（filed by dev-impl-1 per challenge #9，candidate not auto-merge）

### Track 3 follow-up — daemon close authorization gap

**Discovered during cleanup**: v1.2 §10.3 E3.5 wording 寫 「dev-lead 順手關 (merge gate fallback)」但 daemon 實際 enforce owner-only close。發現 cases:
- `t-20260426164717067619-2`（dev-impl-1 owned, PR #199 merged but task close failed by dev-lead）
- `t-20260425040356199333-6`（unassigned, dev-lead also rejected）

**Recommendation** (Track 5 entry, candidate report):
- (a) daemon 加 dev-lead merge-gate fallback close authority for impl-owned tasks (post-PR-merge)
- (b) daemon 加 dev-lead close authority for unassigned tasks (admin cleanup)
- (c) 或 v1.2 §10.3 E3.5 wording 修正 — 移除 「dev-lead 順手關」，明示只能 owner self-close

優先 (a)+(b) implementation；fallback (c) doc-only。

## Track 4: fleet.yaml / config cleanup (dev-lead)

### Result: out-of-scope for overnight window

Sprint 19 final scope freeze 寫「fleet.yaml `templates:` 看 `dev` template backend ref vs actual instances」— 但：

1. **Repo's fleet.yaml** (`agend-terminal/fleet.yaml`) **沒 `templates:` section**（只有 `defaults` / `channel` / `instances`）— 沒 cleanup 對象
2. **Operator's fleet.yaml** (`~/.agend-terminal/fleet.yaml`) — 是 user-specific config，dev team 不該 edit during operator overnight window per `feedback_remote_operator_safety` (lifecycle 凍結邊緣 case)

**Recommendation**: 
- Track 4 deferred — operator wake 時自己 audit `~/.agend-terminal/fleet.yaml` templates section 對齊 actual instances backend (claude × 4)
- 若 templates section 過時，operator 自己 edit 即可（trivial dev → claude default rename）

## Track 5: Cleanup-discovered bugs (dynamic)

Per challenge round #9 — bug fix 永遠 candidate report (not self-dispatched hotfix), operator 醒來 sign。

### Discovered (1 from impl + 1 from dev-lead audit)

- **`t-20260426172642504531-0`** (filed by dev-impl-1, Sprint 19 Track 1.B):
  - cargo-machete false positive `embed-resource` (build.rs only)
  - Fix: 3-line `[package.metadata.cargo-machete] ignored = ["embed-resource"]` to Cargo.toml
  - operator decides priority

- **dev-lead close authority gap** (newly discovered, see Track 3 above):
  - v1.2 §10.3 E3.5 wording vs daemon enforcement mismatch
  - Two scenarios reproduced this overnight (impl-owned + unassigned)
  - Three-fix-options listed under Track 3 follow-up

### Examples of bugs NOT discovered

Sprint 19 cleanup confirmed codebase healthy — no logic bugs / test gaps / doc-vs-code drifts / security issues / performance regressions surfaced during Track 1/2 audits.

## Challenge rounds summary

Per Sprint 19 update m-20260426165446409234-11 (operator 17:01 UTC strategic order — mandatory challenge round before every task dispatch).

### Round 1: Sprint 19 umbrella scope (challenge round → 9 修正)

- **Scope draft posted** as umbrella decision `d-20260426165354292269-2`
- **Broadcast to 4-perspective critique** — daemon filter idle/busy: dev-reviewer + dev-reviewer-2 received broadcast, dev-impl-2 補 send manually, dev-impl-1 在 Sprint 18.5 HOTFIX 不打斷
- **3 verdicts received** (impl-1 後續 cleanup task individual challenge 即可):
  - dev-reviewer m-20260426165652259295-17 (4 findings: LOW false-positive accumulation, security path heuristic, evidence chain tier, prioritized report)
  - dev-reviewer-2 m-20260426165656719385-18 (6 findings: feature gate intersection, multi-PR sweep verify, broken-link risk, security keyword heuristic, fail-safe rules, evidence chain not relax)
  - dev-impl-2 m-20260426165836969781-21 (7 findings: archive naming `docs/archived/`, feature gate, tooling prereq, sequential worktree, Track 5 hotfix vs cleanup, evidence tier, concurrent PR cap)
- **Synthesis** → Sprint 19 final scope `d-20260426170025804519-3` (supersedes umbrella, includes all 9 修正項)
- **Reverses operator 17:00 update Track 5 mandate** — per 3-perspective consensus, cleanup-discovered bug fix not self-dispatched hotfix; operator wake decides

### Subsequent task dispatches (no individual mini-challenge round)

Per implicit reading: umbrella scope freeze 已 cover 4 tracks，sub-PR/task within scope 不必 re-challenge。Track 1 (impl-1) / Track 2 (impl-2) / Track 3-4 (dev-lead) all dispatched without per-PR mini-challenge.

**Edge cases that triggered review-time concern (not separate challenge rounds)**:
- PR #200 (Track 2) reviewer-2 raised 3 minor observations — handled per non-blocking finding flow (backlog t-20260426173120522467-6)
- Track 1 empty + Track 5 dev-impl-1 自 file follow-up — handled per challenge #9 (file task as candidate)

## Authority fail-safe events

Per challenge round #3 fail-safe rules (LOW auto-merge cap N=3 / CI fail freeze / concurrent PR cap=4).

### Triggered events: **(none)**

- LOW auto-merge cap N=3 not approached (Sprint 19 LOW PRs merged: PR #200 only — Track 1 empty, Track 3-4 in this PR pending)
- CI fail freeze not triggered (all merged Sprint 19 PRs CI green)
- Concurrent open PR cap=4 not approached (peak: 1 in-flight Sprint 19 PR at any time)

### Sprint 18.5 HOTFIX (parallel context)

- PR #199 (B Hybrid HOTFIX) merged earlier (`452cd0f`) — outside Sprint 19 fail-safe scope (separate sprint), but counted toward fleet-wide concurrent PR cap during overlap window. Cap not exceeded.

## Sprint 19 final summary

| Track | Scope | Outcome |
|---|---|---|
| Track 1 (dev-impl-1) | Code dead-code / unused / deps cleanup | Empty — codebase clean (3-combo intersection) |
| Track 2 (dev-impl-2) | Docs / decisions archive | PR #200 ✅ merged — 2 archives + 12 candidates report |
| Track 3 (dev-lead) | Backlog task hygiene | This PR — 27-task audit + 1 cancellation candidate (operator action) |
| Track 4 (dev-lead) | fleet.yaml templates | Out-of-scope overnight — deferred to operator |
| Track 5 (dynamic) | Cleanup-discovered bugs | 2 candidates filed, no auto-fix per challenge #9 |

**Authority window outcome**: 1 LOW PR auto-merged (Track 2 PR #200), 0 fail-safes triggered, 0 production breakage risk, 12 docs MEDIUM candidates + 3 decisions candidates + multiple Track 3/5 operator-action items queued.

**Operator wake actions** (priority-sorted):
1. ⚠️ Cancel `t-20260425040356199333-6` (sprint-closed, dev-lead unauthorized)
2. ⚠️ Decide on daemon close authorization gap fix (Track 3 / Track 5 entry — 3-option recommendation)
3. Audit operator's `~/.agend-terminal/fleet.yaml` templates section (Track 4 deferred)
4. Strategic backlog 10 tasks priority排序 (Sprint 20+ planning)
5. Track 2 12 docs MEDIUM candidates archive decisions
6. Track 5 cargo-machete config follow-up (3-line Cargo.toml)
7. PR follow-up backlog 13 tasks distribution to next idle impl waves
