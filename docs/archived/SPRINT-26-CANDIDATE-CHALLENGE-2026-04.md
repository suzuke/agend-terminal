# Sprint 26 candidate challenge — 2026-04

External deep code review surfaced 9 candidate items. Per operator directive (m-105),
dev team ran 4-perspective challenge round. This doc synthesizes verdicts and
recommends Sprint 26 scope for operator approval.

## 4 perspectives

- **dev-impl-1 (minimal/skeptical)**: Why NOT do each, alternatives, YAGNI risk
- **dev-impl-2 (structural)**: Correct architecture, code conflicts, root vs band-aid
- **dev-reviewer (prior-art)**: Past similar attempts, anti-pattern recurrence, deferred-backlog match
- **dev-reviewer-2 (cost/benefit)**: LOC, sprint capacity, dependencies, ROI

## Per-item verdicts

### #1 Split `src/mcp/handlers.rs` 134KB → submodules

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | GO | Real pain (134KB), low risk mechanical refactor |
| Structural | GO Sprint 26 | Mirror `api/handlers/` pattern; ~400 LOC churn 0 net new |
| Prior-art | GO + invariant | Cite `386b98d` (split mcp.rs → mcp/) — split-then-regrew pattern. Pair with `tests/file_size_invariant.rs` capping handlers/* at 500 LOC |
| Cost/benefit | GO Sprint 26 | M-L (~500-800 LOC); UNBLOCKS #5; top-3 ROI |

**Convergence: 4/4 GO Sprint 26 + invariant test.**
**Estimate**: 400-600 LOC + ~50 LOC invariant test. Phase 1 (3 most natural modules) Sprint 26;
phase 2-3 Sprint 27 if needed.
**Dependencies**: UNBLOCKS #5 stable API tier (per-module > per-file 134KB monolith).
**Risk**: Without invariant test, submodules will regrow (cite `386b98d` precedent).

---

### #2 Thread lifecycle registry — `OnceLock<Mutex<HashMap<&'static str, AtomicU32>>>`

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | NO-GO YAGNI | tracing + §10.5 spawn rationale already cover |
| Structural | DEFER Sprint 27 | Counter-only is band-aid. Root: JoinHandle registry + shutdown join order. ~300 LOC + design doc |
| Prior-art | GO Sprint 26 | Pattern verbatim exists at `src/daemon/heartbeat_pair.rs:75`. Cite `8251b78` (PR #235) |
| Cost/benefit | GO Sprint 26 | M (~400-600 LOC); operator-visible 4/5 (`agend-terminal doctor` thread inventory) |

**Convergence: 2 GO / 1 DEFER / 1 NO-GO. SPLIT — scope ambiguity.**

Operator philosophy 「最根本一勞永逸」 forces scope decision:

- **Counter-only (S+M ~400-500 LOC)**: matches reviewer + reviewer-2 estimates. Per impl-2:
  band-aid — count without leak detect.
- **JoinHandle + shutdown join order (L ~600-1000 LOC + design doc)**: root fix per impl-2.
  Sprint capacity tighter; closes structural gap.

**Recommend Sprint 26 GO with JoinHandle scope** (root fix).
Cite `8251b78` (PR #235 HeartbeatPair pattern) + `c25d554` (PR #257 atomic snapshot read).
Pair with DAEMON-LOCK-ORDERING.md doc extension (Level 3 leaf lock per registry).

**Risk**: Scope creep past 1-sprint capacity. Mitigation: ship JoinHandle registry primitive
as PR-A + ~10 highest-traffic spawn site migrations as PR-B; remaining sites Sprint 27.

---

### #3 Backend mode verification — fixtures + CI

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | GO | Real regressions shipped. Start minimal (2 backends) |
| Structural | GO Sprint 26 | Mock backend binary (~50 LOC) > CI matrix (breaks on CLI updates) |
| Prior-art | GO hybrid | F6 audit P3 backlog direct hit. Anti-pattern: real-backend CI matrix fragile; mock = §3.5.10 self-validation trap. Hybrid: mock fast CI + nightly real-Claude |
| Cost/benefit | NO-GO Sprint 26 | L-XL (~1500-3000 LOC + CI infra). Multi-sprint epic. Already P3 backlog |

**Convergence: 3 GO / 1 NO-GO. SCOPE GAP — minimal start vs full epic.**

**Recommend Sprint 26 GO minimal scope** per minimal+structural+prior-art:
- Mock backend binary (~50-100 LOC)
- 2 backend fixtures (claude + kiro-cli) — operator's primary backends
- Hybrid CI: mock for fast verification + nightly real-Claude matrix
- Defer remaining 3 backends + 4-6 modes per backend × 5 backends to multi-sprint epic
- Cancel-or-schedule operator decision on existing P3 backlog `t-20260425040356199333-6`
  (per cost/benefit reviewer-2)

**Estimate Sprint 26**: ~150-300 LOC + nightly CI entry.
**Dependencies**: closes audit F6.

---

### #4 Second maintainer — docs + good-first-issue

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | DEFER | Bus factor matters when there's a team. Sole operator now |
| Structural | GO Sprint 26 | Auto-generate ARCHITECTURE.md from `//!` module docs (~200 LOC) |
| Prior-art | DEFER | Bandwidth real concern. Sprint 25 PR queue depth shows operator+reviewer-2+reviewer bottleneck |
| Cost/benefit | DEFER | Long-term value, zero short-term operator impact |

**Convergence: 3 DEFER / 1 GO. DEFER recommended.**

**Recommend Sprint 26 DEFER**.
Need operator decision on:
- Review-time SLA commitment for external contributors (per prior-art reviewer)
- Sprint 26-27 strategic direction (open-source push or internal stabilization)

Sprint 27+ candidate after #1 lands (per reviewer-2: modular structure → natural good-first-issue).

---

### #5 Stable API subset — tier classification

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | NO-GO | YAGNI. No external consumers. fleet.yaml docs IS stable surface |
| Structural | DEFER Sprint 28 | Needs semver policy. Move core types to lib.rs `#[non_exhaustive]` |
| Prior-art | DEFER | Premature without external API consumer. Revisit when #7 Discord lands (validates trait via second impl) |
| Cost/benefit | DEFER | Strongly blocked by #1. Sprint 27 GO if operator wants 3rd-party integration |

**Convergence: 1 NO-GO / 3 DEFER. DEFER recommended.**

**Recommend Sprint 26 DEFER until external API consumer materializes**.
Strongly blocked by #1 (per-module tier > 134KB monolith).

---

### #6 Mutex poison invariant check — `lock_poisoned_checked` + clippy ban

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | NO-GO | Current `lock_poisoned` + warn sufficient. No incident |
| Structural | DEFER Sprint 28 | Root fix = `parking_lot::Mutex` (no poison). 111 call sites churn |
| Prior-art | GO Sprint 26 + clippy | `src/sync.rs::lock_poisoned()` exists but adopted only ~2 callers. Without clippy ban, helper sits unused |
| Cost/benefit | GO Sprint 26 (top ROI #1) | M (~400-600 LOC). Highest ROI of 9 — failure-rate 5/5 critical prevention. Helper foundation already exists |

**Convergence: 2 GO / 1 NO-GO / 1 DEFER. SPLIT — adoption strategy ambiguity.**

Operator philosophy 「最根本一勞永逸」 forces scope decision:

- **Helper-extension-only (M ~150-300 LOC)**: prior-art + cost/benefit recommended.
  Extend existing `src/sync.rs::lock_poisoned()` to ~111 call sites + clippy invariant test.
  Pragmatic — works with current Mutex.
- **parking_lot migration (XL ~600-1000 LOC + dep change)**: impl-2 structural recommended.
  Eliminates poison entirely. Larger churn + dep update + behavior shift on shutdown panics.

**Recommend Sprint 26 GO helper-extension-only**.
Reasoning: cost/benefit highest ROI + foundation exists + mechanical migration low-risk.
parking_lot migration deferred Sprint 28+ pending operator decision on shutdown-panic semantics.

**Estimate**: ~200-400 LOC. Pair `clippy.toml mutex_lock_unwrap` ban + grep invariant test.

---

### #7 Channel abstraction — Discord/Slack/Matrix

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | DEFER | Demand-driven. Trait ready; impl when operator asks |
| Structural | GO Sprint 27 | Channel trait + contract tests already exist. ~500 LOC |
| Prior-art | GO Sprint 26 Discord ONLY | Cite PR #230 trait surface. Discord IMPL FIRST as second-impl validation BEFORE Slack/Matrix expansion |
| Cost/benefit | NO-GO Sprint 26 | XL (~2000-5000 LOC) multi-sprint epic. Conditional on user base — current evidence operator IS Telegram-only |

**Convergence: 2 GO / 1 DEFER / 1 NO-GO. SPLIT — strategic direction unclear.**

**Recommend Sprint 26 NO-GO awaiting operator strategic decision**.
Operator must answer: are Discord/Slack/Matrix users in scope for Sprint 26-27 timeline?
- If YES: Sprint 27 Discord-only (per prior-art second-impl validation rationale)
- If NO: defer indefinitely

Trait surface ready (PR #230); cost is impl + per-channel real-API capture (audit F10 pattern).

---

### #8 Async PTY I/O

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | NO-GO | Textbook YAGNI. Operator agrees. Comment only |
| Structural | NO-GO | Premature — no operator at >50 agents. ~2000+ LOC rewrite |
| Prior-art | NO-GO | Massive architectural risk (3-6 month rewrite). Theoretical perf without operator-observable bottleneck |
| Cost/benefit | NO-GO | XL+ (~3000-8000 LOC) multi-sprint epic 4-6 sprints. Zero ROI at current scale |

**Convergence: 4/4 NO-GO. UNANIMOUS.**

**Recommend Sprint 26 NO-GO confirmed. Add `// FUTURE` comments only.**
Revisit when fleet exceeds 50+ agents AND operator-observable bottleneck materializes.

---

### #9 Observability — metrics/OpenTelemetry/dashboard

| Perspective | Verdict | Note |
|---|---|---|
| Minimal | DEFER | Single-machine tracing sufficient. Revisit at multi-host |
| Structural | GO Sprint 27 | tracing-opentelemetry subscriber + OTLP export. ~200 LOC |
| Prior-art | DEFER | Single-operator deployment doesn't justify infra cost. Multi-operator would |
| Cost/benefit | DEFER full / phase-1 possible | L-XL (~1500-3000 LOC) multi-sprint; OR phase-1 metrics-only ~500-800 LOC |

**Convergence: 3 DEFER / 1 GO Sprint 27. DEFER recommended.**

**Recommend Sprint 26 DEFER full epic**.
Possible incremental phase 1 (metrics-only ~500-800 LOC) Sprint 27 if operator wants
SLO-tracking visibility before multi-host scale-out.

Operator decision needed: full vs phase-1 scope, AND multi-operator timeline.

---

## Sprint 26 recommended scope

**3-PR parallel wave** (~1100-1500 LOC total, fits 2-week sprint with 2 dev-impl agents):

### PR-A: #6 mutex poison invariant + clippy ban (~200-400 LOC)
- Extend `src/sync.rs::lock_poisoned()` to all `.lock()` sites (~111 callers)
- `clippy.toml mutex_lock_unwrap` ban + grep invariant test
- §3.5.10 concurrent-state class fixture
- §3.5.11 test-first dogfood
- **Cite**: highest ROI per cost/benefit reviewer-2; foundation exists per prior-art reviewer

### PR-B: #2 thread lifecycle registry — JoinHandle + shutdown join order (~600-1000 LOC)
- Primitive: `OnceLock<Mutex<HashMap<&'static str, JoinHandle>>>`
- Doctor integration: `agend-terminal doctor` reports active threads
- Shutdown join order: graceful join in lifecycle teardown
- DAEMON-LOCK-ORDERING.md doc extension (Level 3 leaf per registry)
- §3.5.10 concurrent-state class — thread harness OR Option 4 deterministic
- **Risk**: scope creep. Mitigation: ship primitive PR-B1 + 10 highest-traffic site migrations PR-B2; remaining sites Sprint 27
- **Cite**: PR #235 (`8251b78`) HeartbeatPair pattern + PR #257 (`c25d554`) atomic snapshot read

### PR-C: #1 mcp/handlers.rs split phase 1 (~400-600 LOC)
- 3 most natural modules (instance / inbox / tasks per impl-2; or channel / messaging / instance per impl-2 alt)
- Mirror `src/api/handlers/` pattern
- `tests/file_size_invariant.rs` capping handlers/* at 500 LOC (per prior-art reviewer)
- §3.5.10 wire-format preserved (post-split run mcp_bridge_client_handshake + mcp_proxy_behavioral_parity)
- Multi-PR continuation Sprint 27 (remaining modules)

---

## Sprint 26 scope NOT included

- **#3 Backend verification**: Sprint 26 minimal scope possible (mock + 2 backends ~150-300 LOC) IF operator approves cancelling existing P3 backlog `t-20260425040356199333-6` and rescheduling. **Operator decision required**.
- **#4 / #5 / #7 / #9**: DEFER — operator strategic direction needed
- **#8 async PTY**: NO-GO confirmed unanimous

---

## Capacity assessment

3-PR wave fits 2-week sprint:
- PR-A: ~3-5 days, dev-impl-2 (continues mcp context from PR #262)
- PR-B: ~5-8 days (JoinHandle scope), dev-impl-1 (continues api/mod.rs context from PR #263)
- PR-C: ~3-5 days, dev-impl-1 OR dev-impl-2 phase 1; phase 2-3 Sprint 27

If operator opts for #3 minimal addition, capacity tightens. Recommend either:
- Drop PR-C to Sprint 27 (delays #5 unblock by 1 sprint)
- OR extend Sprint 26 to 3 weeks

---

## Operator decisions required

1. **#2 scope**: Counter-only (band-aid) OR JoinHandle + shutdown join (root fix)?
   Recommended: JoinHandle root fix per philosophy.
2. **#3 P3 backlog**: Cancel existing `t-20260425040356199333-6` and reschedule minimal Sprint 26 (~150-300 LOC)?
   OR keep deferred indefinitely?
3. **#7 strategic**: Discord/Slack/Matrix users in Sprint 26-27 timeline scope?
4. **#9 phasing**: Defer full epic OR ship phase-1 metrics-only Sprint 27?
5. **#6 scope**: helper-extension-only OR parking_lot migration?
   Recommended: helper-extension-only Sprint 26; parking_lot Sprint 28+ pending shutdown-panic-semantic decision.

---

## Backlog updates after Sprint 26 ships

- #1 phase 2-3 → Sprint 27 (after phase 1 lands)
- #5 stable API tier → Sprint 27 (unblocked by #1)
- #4 second maintainer → Sprint 27+ (after modular structure surfaces good-first-issue)
- §3.5.11 deletion-PR exemption refinement (per PR #262 reviewer-2 m-79) → Sprint 26 P3 backlog

---

## Cross-amendment dependencies

All 3 Sprint 26 PRs ship under §3.5.10 + §3.5.11 + §3.5.12 + §3.5.13 enforcement:
- §3.5.10: external-fixture (3 classes per amendment) per PR
- §3.5.11: test-first commit-order strict (PR #262 r1→r2 precedent)
- §3.5.12: deferred-defense process gates (don't add new defers without dual + operator post_decision)
- §3.5.13: verdict externalization (mirror to GH PR comment)

PR-B (thread registry) likely surfaces §3.5.10 r3 amendment refinement (per PR #261 not-Send + in-memory pattern; thread JoinHandles could trigger third class).

---

## Source verdicts (4 perspectives)

- impl-1 minimal m-110 (2026-04-28 00:31)
- impl-2 structural m-111 (2026-04-28 00:32)
- reviewer prior-art m-112 (2026-04-28 00:34)
- reviewer-2 cost/benefit m-113 (2026-04-28 00:34)
