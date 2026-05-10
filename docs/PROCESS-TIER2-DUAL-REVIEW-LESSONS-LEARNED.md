# Tier-2 dual review — lessons learned + dispatch criteria

**Sprint 63 W2 PR-1 — process protocol.** Closes Sprint 58 P2 #7
deferred carryover by capturing Pass 2 framing rationale from Sprint
57 Phase 3 #557 and Sprint 57 Track C dual-review precedents. Pairs
with `PROCESS-LOC-ESTIMATION-METHODOLOGY.md` (Sprint 60 #582) and
`PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md` (Sprint 61 #588) as the
third process doc in the Sprint 60-63 protocol-formalization arc.

---

## 1. Why this exists

Tier-2 dual-review is the escalation tier above Tier-1 single-primary.
It costs ~2x reviewer wall time and ~1.5x adjudication overhead.
Misapplied (escalating Tier-1-appropriate work) it slows the wave;
under-applied (single-primary on genuinely cross-cutting risk) it
risks shipping correctness gaps that single-vantage review misses.

Sprint 57 produced two precedent dual-review cycles whose Pass 1 →
Pass 2 transitions captured useful framing. This doc surfaces the
trigger criteria + adjudication patterns so future dispatchers don't
re-derive the framing case-by-case.

---

## 2. Sprint 57 precedent cycles

### 2.1 Sprint 57 Phase 3 #557 (cross-platform service install/uninstall)

- **Pass 1 framing**: Tier-1 single-primary review. Reviewer flagged
  failure-mode-3 wording precision in ci_watch narrative + raw-path
  XML escape gap.
- **Pass 2 framing**: dispatch escalated to Tier-2 dual-review for
  `r2` because the cross-platform fork (launchd plist + systemd unit
  + Windows Task Scheduler XML) hit Class-A risk (raw `&` in path →
  malformed plist → launchctl rejects). Dual-review caught the
  format-aware escape gap that single-vantage missed.
- **Outcome**: r2 merged after Pass 2 cohesion-accept. Format-aware
  escape helpers (`xml_escape` + `systemd_quote`) shipped as the
  generalized fix.

### 2.2 Sprint 57 Track C #553 (dedup-state persistence)

- **Pass 1 framing**: Tier-1 single-primary. Reviewer flagged
  schema-evolution forward-compat + tmp-file orphan accumulation as
  non-blocking.
- **Pass 2 framing**: Track C r2 escalated to Tier-2 dual-review when
  the schema migration story spanned daemon / supervisor / event-log
  emission paths simultaneously. Dual-review caught the
  schema-version skip-on-mismatch contract that single-vantage
  framed as "log warn + drop" but actually needed structured
  forward-compat semantics for downgrade scenarios.
- **Outcome**: r2 merged with documented schema-evolution contract
  (forward-only with upgrade-time skip-on-mismatch) as the load-
  bearing addition.

---

## 3. Tier-2 dual-review trigger criteria

Trigger Tier-2 dispatch (vs default Tier-1) when ANY of the following
applies:

1. **Cross-platform fork ≥ 3 platforms with diverging behaviour** —
   e.g. macOS + Linux + Windows service install. Single-vantage
   reviewer typically depth-reviews one platform; the other two get
   surface-pass which misses Class-A escape / quoting / path-format
   gaps. Sprint 57 Phase 3 #557 precedent.
2. **Schema migration spanning ≥ 2 subsystems** — e.g. on-disk
   schema bump + supervisor in-memory state + event-log emission +
   downstream consumer expectations. Sprint 57 Track C #553 precedent.
3. **Class-A risk surface** — defined as: silent data corruption,
   security boundary, or process-restart blast-radius. Tier-2's
   second vantage halves the residual risk of subtle gaps slipping
   through.
4. **Surface-block / API-gap revealed mid-PR** — when a PR's
   investigation reveals an upstream API gap (e.g. teloxide forum-
   topic enumeration in Sprint 59 #574) or in-flight scope reframing
   (e.g. F2 path reduction). Dual-review confirms the reduced scope
   is correctly bounded.

If none of these apply, Tier-1 single-primary is the correct default
(per established precedent — 60+ Sprint 56-63 PRs Tier-1-shipped
clean).

---

## 4. Adjudication patterns

### 4.1 Pass 1 → Pass 2 transition

When a Tier-1 reviewer flags non-blocking concerns AND the PR
warrants Tier-2 escalation per §3, lead may dispatch Pass 2 framing:

- **Cohesion-accept Option (a)**: Pass 2 reviewer adjudicates that
  the cohesive single-PR holds vs split. See `PROCESS-LOC-ESTIMATION-METHODOLOGY.md`
  §5.4 for the LOC-overage variant; the same adjudication semantics
  apply to scope-overage from genuinely cross-cutting concerns.
- **Reduced-scope rebuild**: Pass 2 reframes scope reduction (e.g.
  Sprint 59 W2 PR-IMPL F2 path reduction from 5-class → 4-class
  taxonomy when teloxide enumeration unavailable). Pass 2 reviewer
  verifies the reduction preserves correctness invariants.
- **Cross-platform format-aware fix**: Pass 2 reviewer brings the
  second platform's vantage. Often surfaces format-specific gaps
  (e.g. XML entity escape for plist + Task Scheduler vs systemd
  shell quoting for ExecStart).

### 4.2 Cohesion-accept reuse from Tier-1

The cohesion-accept option (a) override pattern documented in
`PROCESS-LOC-ESTIMATION-METHODOLOGY.md` §5.4 is fully applicable to
Tier-2 dual-review verdicts. Pass 2 reviewer can apply cohesion-
accept exactly as Tier-1 reviewer would. Pattern uniformity across
review tiers — no separate Tier-2 override mechanic.

---

## 5. Cross-references

- `PROCESS-LOC-ESTIMATION-METHODOLOGY.md` (Sprint 60 #582) —
  cohesion-accept option (a) override mechanic + soft-warn / hard-
  fail thresholds.
- `PROTOCOL-PARALLEL-FILLER-OPT-IN-SCHEMA.md` (Sprint 61 #588) —
  parallel-filler dispatch protocol; related to but distinct from
  Tier-2 escalation (parallel-vs-sequential dispatch independent of
  review-tier choice).
- `PROCESS-LEAD-CLOSEOUT-CLAIM-STATE-DISCIPLINE.md` (Sprint 59 #569)
  — closeout synth claim-state discipline; pairs with Tier-2 verdict
  delivery for high-confidence closeout reporting.

---

## 6. Out of scope (Sprint 64+ candidates)

- **Tier-3 / triple-review escalation** — for genuinely
  catastrophic-risk surface (e.g. crypto-implementation,
  authentication-protocol, or daemon-replace-self code paths).
  Sprint 64+ if a candidate surface emerges; current Sprint 56-63
  workload has not produced a Tier-3 case.
- **Automated Tier classification** — dispatch-time tooling that
  scans predicted file touches + scope-overage + cross-platform
  fork count to suggest Tier-1 vs Tier-2. Pure-wiring follow-up
  once #582-style automation has more uptake data.
- **Reviewer-rotation policy** — same primary reviewer for both Pass
  1 and Pass 2 vs different reviewer per Pass. Current default is
  same primary; Sprint 64+ if pattern suggests rotation reduces
  cohesion-accept-fatigue.

---

**Summary.** Tier-2 dual-review costs ~2x reviewer wall + ~1.5x
adjudication overhead. Trigger criteria: cross-platform fork ≥ 3
diverging platforms, schema migration spanning ≥ 2 subsystems,
Class-A risk surface, or surface-block / scope-reframe revealed
mid-PR. Adjudication reuses cohesion-accept option (a) from #582;
no separate Tier-2 mechanic. Sprint 57 Phase 3 #557 + Sprint 57
Track C #553 are the load-bearing precedents. Tier-3 / automated
classification / reviewer rotation deferred Sprint 64+.
