# Task Board Auto-Close Redesign

**Status**: Synthesis complete, pending operator approval. Per decision `d-20260427062051439951-1`.
**Predecessor**: Operator directive 2026-04-27 06:08 UTC ("最優雅 + 最根本 + 一勞永逸") routed via general m-20260427061511246589-0.
**Authority**: 4-perspective challenge round per v1.2 amendment #1 (mandatory pre-design dispatch).

---

## Problem statement

PR-AY (Sprint 18) auto-close 3 conditions ALL-required:
1. Task status = `verified` (manual transition after reviewer VERIFIED)
2. PR branch name token-matches task title/description
3. PR watched via `watch_ci`

**Operational reality** (Sprint 19 audit + Sprint 22 wrap observation):
- Most sprint tasks never reach `verified` state — lifecycle skips on workflow rather than discipline
- Branch token-match fragile: `§` chars, abbreviations, snake/kebab variance, multi-token PRs
- Legacy 28 backlog (pre-PR-AY) never lifecycled in → permanently stale
- Task ownership rule blocks `system caller` cleanup

**Operator trust signal**: ground-truth on board has been LOW since Sprint 19 backlog audit revealed 6/31 tasks "actually done" while remaining 25 lingered.

---

## Operator constraints (NOT negotiable)

1. 最優雅 + 最根本 + 一勞永逸 — NO incremental patch adding more conditions
2. One mechanism handling 99%+ cases
3. Retroactive works on legacy 28 backlog (no manual sweep)
4. Doesn't require new protocol amendment (avoid §3.5.5 amendment debt cycle)
5. Operator can audit easily (transparent, not hidden daemon logic)

---

## 4-perspective challenge round outcomes

| Vantage | Recommendation | Key argument |
|---|---|---|
| **impl-1** (implementer-A) | **F** (D+C hybrid + CI gate + retire `verified`) | "GitHub IS source-of-truth, board syncs GitHub" — pragmatic leverage of existing webhooks |
| **impl-2** (implementer-B alt-path) | **J** (event-sourced append-only `task_events.jsonl`) | F7 atomic-write lesson transfer — append-only naturally atomic, no partial-write window; mirrors my own `fleet_events.jsonl` pattern from PR #199 |
| **dev-reviewer** (reviewer-readiness) | **D** (or D-only F-lite) | Anti-bypass invariant pattern collapse: `git log --grep="Closes t-"` IS audit IS invariant; immutable, no daemon mediation, operator-runnable in one shell command |
| **dev-reviewer-2** (ADVERSARIAL cross-vantage) | **C** (pull-model sweep) | "Boring under adversity" — agent-prompt-injection-immune + GitHub-partition-tolerant + bounded staleness; D's "elegance IS fragility" |

### Convergent areas

- **Sweep is needed for retro-active legacy 28** (impl-1 + impl-2 + dev-reviewer-2 all agree D-alone cannot retro-act on immutable PR history)
- **Type-system / invariant enforcement is the test gate** (impl-2 + dev-reviewer both call out anti-bypass pattern from Sprint 21+22)
- **Forensic trail mandatory** (dev-reviewer + dev-reviewer-2 both call out audit observability)

### Critical adversarial findings (dev-reviewer-2 deep-dive on D)

D-as-canonical fails 4 adversarial dimensions:

1. **Owner-authorization gate violated (E3.1)**: typo `Closes t-victim` silently wrong-closes another agent's task; no owner constraint between PR author and task owner; **re-introduces pre-PR-220 decisions::update bug class** that Sprint 21 Phase 2 D1 just closed
2. **Retro-active to legacy is structurally impossible**: legacy 28 PRs are immutable history with no markers
3. **Post-merge body edit chaos**: GitHub allows PR description edits indefinitely; daemon must either freeze-at-merge-time (custom plumbing) or accept retroactive close mutations (unbounded surprise)
4. **Adversary control surface**: prompt-injected agent writes `Closes t-victim-N` in PR body to close someone else's task; PR description is plain text adversaries control

**dev-reviewer-2 verdict**: "D's elegance IS the fragility."

### Critical adversarial findings (dev-reviewer-2 on B)

B-as-source-of-truth-inversion presupposes every task is a code change:

- **GitHub-as-SPOF**: 4-hour incident during sprint planning at 09:00 destroys the model permanently in operator's mind
- **No-PR tasks**: infrastructure / audit / decision / scope-freeze tasks have no PR mapping; B-pure forces every task into PR shape, distorting board
- **Closed-without-merge ambiguity**: rejected vs withdrawn vs superseded vs abandoned — B doesn't have enough signal in PR state alone

---

## Trade-off matrix

| Option | Cost (LOC) | Coverage | Failure mode | Retro-active | Operator audit | Adversary surface | Trust 6mo |
|---|---|---|---|---|---|---|---|
| A. Forward-only strengthen | 200-400 | < 99% | bifurcated cohorts | ❌ | medium | low | degrading |
| B. Source-of-truth inversion | 1500-2500 + protocol amendment | depends on GitHub | GitHub-SPOF | ✓ (with rewrite) | medium | distributed | degrading fast |
| C. Pull-model sweep | 300-500 | ✓ 99%+ | bounded staleness | ✓ (by construction) | high (sweep logs) | low (daemon-driven) | **stable** |
| D. PR body embed task IDs | 150-250 | < 99% | typo/forge/edit | ❌ (legacy no markers) | high (`git log --grep`) | **highest** (adversary text) | degrading |
| E. Lifecycle simplification | 50 | n/a — orthogonal to close mechanism | trust transferred to social contract | ❌ | low | low | degrading slowly |
| F. Hybrid D+C | 400-600 | ✓ 99%+ | dual-mutator conflict | ✓ via C | mixed | inherits D's surface | degrading mid-term |
| **J. Event-sourced append-only** (new from impl-2) | 80 (storage) | n/a — needs trigger | event-arrival lag | ✓ via replay | **highest** (JSONL log) | low | **stable** |

---

## Recommended design: C + J

**Single canonical mechanism (C) using single source-of-truth storage (J)**.

### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│  Daemon (single mutator)                                    │
│                                                              │
│  ┌──────────────┐   ┌─────────────────────────────────┐    │
│  │ Sweep tick   │──▶│ GitHub poll (auth via TOKEN)    │    │
│  │ (cron, 5min) │   │ - PR list (open/merged/closed)  │    │
│  │              │   │ - branch heads                  │    │
│  │              │   │ - merge timestamps              │    │
│  └──────────────┘   └─────────────────────────────────┘    │
│         │                       │                            │
│         │                       ▼                            │
│         │          ┌─────────────────────────────────┐      │
│         │          │ State diff (vs current board)   │      │
│         │          │ - new merges → PrMerged events  │      │
│         │          │ - closed-no-merge → no event    │      │
│         │          │ - reverted → Reopened events    │      │
│         │          └─────────────────────────────────┘      │
│         ▼                       │                            │
│  ┌──────────────────────────────▼─────────────────────┐    │
│  │ task_events::append() (single mutation point)      │    │
│  │ Anti-bypass invariant test enforces this is the    │    │
│  │ ONLY writer to task_events.jsonl                   │    │
│  └────────────────────────────────────────────────────┘    │
│                       │                                      │
│                       ▼                                      │
│  ┌────────────────────────────────────────────────────┐    │
│  │ task_events.jsonl (append-only, immutable)         │    │
│  │ Schema: {timestamp, task_id, event, source_evidence}│   │
│  └────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────┘

Board state at query time = fold(task_events.jsonl)
Operator audit = grep task_id task_events.jsonl
```

### TaskEvent enum (type-system enforcement per impl-2 + dev-reviewer praise patterns)

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "kind")]
pub enum TaskEvent {
    Created { task_id: String, owner: String, title: String, description: String },
    Claimed { task_id: String, by: String },
    InProgress { task_id: String, by: String },
    Verified { task_id: String, by_reviewer: String, verdict: String },
    Done { task_id: String, by: String, source: DoneSource },
    Reopened { task_id: String, reason: String, source_evidence: String },
    SweepStarted { tick_id: String, github_state_snapshot: String },
    SweepCompleted { tick_id: String, events_emitted: u32, duration_ms: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "via")]
pub enum DoneSource {
    PrMerged { pr_number: u32, merge_sha: String, merged_at: String },
    OperatorManual { authored_at: String },
    LegacyBackfill { sweep_id: String, reasoning: String },
}
```

Exhaustive `match` on `TaskEvent` and `DoneSource` is compiler-enforced — adding a new state requires adding a match arm everywhere it's consumed (mirrors PR #220 `Terminal { merged: bool }` + PR #230 `OutboundCapabilityDecision` patterns).

### Anti-bypass invariant test

`tests/task_events_invariant.rs`:

```rust
// Mirrors legacy_outbound_path_audit.rs (Sprint 22 P0) + spawn_rationale_audit.rs (Sprint 21 Phase 5)
#[test]
fn task_state_mutations_only_via_task_events_append() {
    // Source-grep all .rs files in src/ for direct writes to tasks.jsonl
    // EXEMPTED_LEGACY_CALLERS = []  (empty by intent — anti-growth contract)
    // Failure: any module bypassing task_events::append() = test fails-loud
    // ...
}
```

### Sweep validation (closes dev-reviewer-2 D adversarial gap)

When sweep observes `Closes t-XXX-N` in PR body (D pattern as advisory hint):
1. Validate `t-XXX-N` exists in current task board
2. Validate PR author == task owner OR PR author in task team
3. Validate PR actually merged (not draft, not closed-no-merge)
4. Validate task is currently `open` or `in_progress` (not already done)
5. If ALL pass → emit `Done { source: PrMerged{pr_number, merge_sha, merged_at} }`
6. If ANY fail → emit no event, log to sweep diagnostic

Adversary writes `Closes t-victim-N` in PR body → step 2 fails (author ≠ task owner) → no event → operator audit shows sweep saw the marker but rejected it.

### Operator audit primitives

| Question | Command |
|---|---|
| "Why is t-X-Y done?" | `grep '"task_id":"t-X-Y"' task_events.jsonl` (full event chain) |
| "What did sweep do at 12:00?" | `jq 'select(.kind == "SweepCompleted") \| select(.timestamp > "12:00")' task_events.jsonl` |
| "Show all force-closes by source" | `jq 'select(.event.via == "OperatorManual" or .event.via == "LegacyBackfill")' task_events.jsonl` |
| "Was sweep healthy this hour?" | `grep "SweepCompleted" task_events.jsonl \| tail -12` (expect 12 ticks at 5min interval) |

---

## Migration plan for legacy 28 backlog

### Phase 1: Dry-run sweep
- Sweep walks all 28 open tasks
- For each, queries GitHub for matching closed/merged PRs (heuristic: branch name contains task title token OR PR body contains task ID OR PR-AY's existing branch token-match passes)
- Produces candidate-close report: `(task_id, candidate_pr, merge_evidence, confidence_score)`
- **No mutations applied** — report posted as decision

### Phase 2: Operator-confirm gate
- Operator reviews candidate report
- Approves with high-confidence threshold (e.g., confidence >= 0.8 = auto-apply, < 0.8 = manual review)
- Or operator may bulk-cancel low-confidence candidates as "no PR matches, mark as stale-cancel"

### Phase 3: Sweep applies legacy backfill
- Sweep emits `Done { source: LegacyBackfill { sweep_id, reasoning } }` events for approved candidates
- Forensic trail preserved: every legacy close event includes the sweep_id and reasoning prose

### Phase 4: Anti-growth contract
- Legacy backfill is one-time sweep
- Subsequent sweeps only operate on tasks created post-cutover
- Invariant test: any `LegacyBackfill` event after cutover sweep = fail (compile-time check via cutover sweep_id whitelist)

---

## Edge case enumeration

| # | Edge case | Handling |
|---|---|---|
| 1 | PR closed-without-merge (rejected/withdrawn) | Sweep emits NO event; task stays open per E3.1 owner-mutation expectation |
| 2 | PR merged via revert-merge | Sweep emits `Done` then potentially `Reopened` event chain on revert detection |
| 3 | Force-pushed past auto-close commit | Sweep validates current main HEAD; immutable JSONL log preserves prior `PrMerged` event for audit |
| 4 | ~~Hot-reload `fleet.yaml` mid-sweep~~ | N/A as of Sprint 29 PR-6 — hot-reload removed; fleet.yaml edits require daemon restart |
| 5 | Operator manual `task done` while sweep in-flight | Append-only naturally handles via timestamp ordering; idempotency check in `task_events::append()` (re-close = no-op + log) |
| 6 | Legacy 28 replay determinism | Events ordered by PR mergedAt timestamp, deterministic replay regardless of sweep run order |
| 7 | Sweep failure (network, GitHub 5xx, panic) | Heartbeat event missing in next tick → operator sees gap → manual `task list --diagnose` runs ad-hoc sweep |
| 8 | GitHub rate limit (5000/hr auth) | Sweep backoff per Sprint 21 PR-AP rate-limit pattern; sleep-with-jitter exponential |
| 9 | PR body contains `Closes t-X-Y` typo (`t-X-Z`) | Sweep validation step 1 fails (task t-X-Z doesn't exist) → no event → log advisory |
| 10 | PR author writes `Closes t-victim-N` to attack | Sweep validation step 2 fails (author ≠ task owner) → no event → log advisory |
| 11 | Same author opens 5 PRs all citing t-X-Y in body | First valid sweep emits `Done`; subsequent sweeps see task already done, validation step 4 fails, no duplicate events |
| 12 | Daemon crash mid-event-write | Append-only JSONL is line-atomic at filesystem level (POSIX atomic for line buffers <4KB) — no partial-event class |
| 13 | task_events.jsonl grows unbounded | Compaction policy: keep last N=10000 events in hot file, archive older to `task_events.YYYY-MM.jsonl`; query layer reads both |
| 14 | Operator wants to "force open" a task that sweep marked done | Operator manual `task open --force t-X-Y` emits `Reopened { reason: <required>, source_evidence: <required> }` event |

---

## REJECT criteria (per dev-reviewer)

For implementation PR:
1. **Forensic trail mandatory** — every event includes `(task_id, mechanism, source_evidence_ptr)` 
2. **Unit tests** for state-transition correctness (PR #233 dual-coverage pattern)
3. **Retro-active correctness on legacy 28 verifiable on PR review surface** (NOT deferred to post-merge sweep)
4. **Idempotency** (re-close = no-op, no event spam, no state churn)
5. **No silent open-state paths** (every "stays open" branch has observable signal)
6. **Single mechanism** (parallel close paths removed in same PR — no D-as-canonical co-existence)

---

## Why this is 最優雅 + 最根本 + 一勞永逸

| Operator constraint | How design satisfies |
|---|---|
| **最優雅** | 1 mechanism (C sweep) + 1 storage (J append-only) + 1 invariant test (anti-bypass grep) = clean architecture; reuses Sprint 21+22 patterns |
| **最根本** | Removes BOTH (a) PR-AY's 3-condition stack (`verified` requirement gone, branch token-match downgraded to advisory hint, watch_ci no longer load-bearing) AND (b) legacy backlog cohort split (sweep walks all open tasks regardless of cohort) |
| **一勞永逸** | Append-only log = no partial-write class possible (F7 lesson); daemon-driven = no agent-injection class possible; bounded staleness = no surprise failure mode; type-system enforced = no future regression class |
| **One mechanism handling 99%+** | Sweep is the only mutator; D-pattern PR body markers are advisory hints validated by sweep, not parallel mutators |
| **Retro-active legacy 28** | Sweep walks all open tasks by construction; legacy backfill = one-time sweep with operator-confirm gate |
| **No protocol amendment** | Implementation purely in `src/tasks.rs` + new `src/task_events.rs` + tests; doesn't change task lifecycle semantics in protocol doc |
| **Operator audit transparent** | `grep t-X-Y task_events.jsonl` answers any audit question in single shell command; JSONL is human-readable |

---

## Implementation sprint scope (Sprint 24 P0 candidate, pending operator approval)

- **LOC estimate**: J primitive ~80 + C sweep ~150 + invariant test ~100 + legacy backfill tool ~50 + tests ~150 + USAGE.md migration section ~40 = **~570 LOC**
- **Files touched (new)**:
  - `src/task_events.rs` (TaskEvent enum + append/fold)
  - `tests/task_events_invariant.rs` (anti-bypass + replay determinism)
  - `tests/legacy_backfill_dry_run.rs` (legacy 28 audit verifiable on PR review)
- **Files touched (modified)**:
  - `src/tasks.rs` (mutation paths route through task_events::append())
  - `src/daemon/cron_tick.rs` OR new `src/daemon/task_sweep.rs` (sweep tick implementation)
  - `src/mcp/handlers.rs` (task tool MCP arms emit events instead of direct mutation)
  - `docs/USAGE.md` (operator audit primitives + sweep cadence)
- **Reviewer**: dev-reviewer Tier-2 auto-Critical (touches `src/tasks.rs` + new `src/task_events.rs` + invariant)
- **Cross-team**: dev-reviewer-2 standby for adversarial cross-vantage second-pass on sweep validation logic
- **4-perspective challenge round mandatory** pre-implementation-dispatch (per amendment #1) — this design synthesis is the planning round, separate impl-dispatch round needed before code

---

## Open questions for operator approval

1. **Sweep cadence**: 5min poll default acceptable? Or shorter (1min, faster freshness, more API cost) vs longer (10min, slower freshness, lower API cost)?
2. **Legacy backfill confidence threshold**: confidence >= 0.8 auto-apply OR every legacy backfill requires operator manual review?
3. **PR body marker policy**: enforce CI gate (every sprint task PR must contain `Closes t-XXX-N` in body, fails CI otherwise) OR keep advisory-only?
4. **Compaction**: keep last 10000 events in hot file acceptable? Or different threshold?
5. **Implementation sprint priority**: Sprint 24 P0 (this) OR defer to Sprint 25 (would push docs sprint to Sprint 25 too)?

---

## Decision

This design is the synthesis of 4-perspective challenge round per v1.2 amendment #1. **Pending operator approval** before implementation dispatch.

Per general routing m-20260427061511246589-0: this report is the deliverable. Operator reviews → approves design → dev-lead dispatches implementation sprint with separate 4-perspective challenge round on impl-time decisions (test scaffolding, sweep concurrency model, legacy backfill UI).
