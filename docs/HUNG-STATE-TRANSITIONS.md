[繁體中文](HUNG-STATE-TRANSITIONS.zh-TW.md)

# Hung-State Transition Audit

> **CURRENT-STATUS NOTE (`main@1d83b423`, 2026-07-16).** This file preserves
> the #685 Phase-1 transition audit and its stable section anchors. The live
> source is authoritative: state tracking moved from `src/state.rs` to
> `src/state/mod.rs` + `src/state/patterns.rs` / `src/backend_profile.rs`;
> Gemini is retired (Agy is its successor, and Grok is now supported); and the
> old "warn only, no recovery consumer" conclusion is superseded. Current
> `check_hang` wiring is in
> `src/daemon/per_tick/hang_detection.rs`, followed in the same canonical
> handler pipeline by the Stage-1-only recovery dispatcher. Dispatcher Stages
> 2/3 were removed in #2549; see [RECOVERY-STAGES.md](RECOVERY-STAGES.md).
> Historical Gemini patterns and hypotheses below remain provenance, not
> guidance for current backend tuning.

Contract baseline for `HealthState::Hung` and `HealthState::IdleLong`
transition semantics in `src/health.rs`. Companion to inline structured
comments at each mutation site and the `check_hang` function-level rustdoc.

Issue: [#685](https://github.com/suzuke/agend-terminal/issues/685) Phase 1
deliverable #1. Decision: `d-20260513154400110972-2`. Scope is strict — see
`§Scope` below.

Maintenance: section IDs (`§Entry.E1`, `§Exit.X1`, etc.) are **contract**
anchors — renaming any heading is a PR-scope-break that must propagate to
inline comments + decision references. Cross-references in this doc to
source use `rg <pattern>` grep hints rather than file-line refs, so refactor
that re-flows lines does not invalidate this doc; line refs in the prose
below are illustrative-only and reflect HEAD `2f24376`.

The F9 productive-output contract is consolidated into §F9.1–§F9.5 below.
Those sections are the maintained source for the productive-output gate;
the former standalone F9 document is only a consolidation input.

## Lifecycle overview

```
                      ┌──────────────┐
                      │   Healthy    │◄────────────────┐
                      └──────┬───────┘                 │
                             │ record_crash            │ §Exit.X1
                             ▼                         │ silence drops
                      ┌──────────────┐                 │
                      │  Recovering  │                 │
                      └──────┬───────┘                 │
                             │ recent ≥ 3              │
                             ▼                         │
                      ┌──────────────┐                 │
                      │   Unstable   │                 │
                      └──────┬───────┘                 │
                             │ total_crashes ≥ max     │
                             ▼                         │
                      ┌──────────────┐                 │
                      │    Failed    │                 │
                      └──────────────┘                 │
                                                       │
   ┌───────────────────────────────────────────────────┴──────────────┐
   │                                                                  │
   │  check_hang mutator monopoly (§Invariants 5b)                    │
   │                                                                  │
   │  silence > threshold ──┬── input pending past hb ──► §Entry.E1   │
   │                        │                                          │
   │                        ├── heartbeat fresh ──────► §Entry.E2     │
   │                        │                                          │
   │                        └── neither ──────────────► §IdleLong.E1  │
   │                                                                  │
   │  state ∈ {Hung, IdleLong}, silence drops below ─► §Exit.X1       │
   │                                                                  │
   └──────────────────────────────────────────────────────────────────┘

   ErrorLoop (separate state) — see §Open questions
```

## Scope

In-scope state mutations (audited below):

- `HealthState::Hung` Entry (E1, E2) and Exit (X1)
- `HealthState::IdleLong` Entry (E1) and Exit (X1, shared predicate with Hung)

Out-of-scope (explicit):

- `HealthState::Healthy / Recovering / Unstable / Failed / ErrorLoop` transitions —
  not driven by `check_hang` (see §Invariants 5b). Audited elsewhere.
- `AgentState` (in `src/state.rs`) — F39 evidence lives there but is referenced
  only via the §F39 cross-reference table below; not mutated by this scope.

## Invariants

These hold at HEAD `2f24376` and are forward-locked by decision
`d-20260513154400110972-2`:

- **5a (exhaustive entries)** — `HealthState::Hung` has exactly **two**
  entry sites; `rg "self\.state = HealthState::Hung" src/health.rs` returns
  exactly two matches (`§Entry.E1` and `§Entry.E2`, both inside
  `check_hang`). No third entry path exists.

- **5b (mutator monopoly)** — Every read/write of `HealthState::Hung` lives
  inside `check_hang`. `maybe_decay` (`rg "fn maybe_decay" src/health.rs`)
  mutates only `Failed → Recovering` and `Unstable → Healthy`; F10 verified.
  Implication: a reader auditing Hung semantics needs to read exactly one
  function.

- **5c (wire-compatible external surface)** — External consumers of the
  Hung state are the bool returned by `check_hang` (driven by
  `rg "check_hang" src/daemon/mod.rs`, sole consumer is a `tracing::warn!`)
  and the `display_name()` string serialized by `rg "health_state" src/api/handlers/query.rs`
  and `rg "health_state" src/mcp/handlers/instance.rs`. **No external code
  pattern-matches on the `HealthState::Hung` variant.** Implication:
  follow-up sub-tasks (F9 / F10 / F39) can change Hung internal semantics
  wire-compatibly as long as the `check_hang -> bool` and
  `display_name()` contracts hold.

- **5d (negative invariant — `maybe_decay` does not touch Hung)** — F10
  audit confirmed: `maybe_decay` reads `last_crash.elapsed()`, not
  `last_output.elapsed()`. Its state mutations are scoped to
  `Failed → Recovering` and `Unstable → Healthy`. **It will never exit
  Hung.** A Hung agent stays Hung until `check_hang` itself observes
  silence dropping below threshold (`§Exit.X1`). This negative invariant
  is duplicated in the `check_hang` function-level rustdoc augmentation
  for proximity to the audience that cares.

## Entry transitions

### §Entry.E1 — input pending past heartbeat

- **Find in source**: `rg "Hung Entry \(E1\)" src/health.rs`
- **PRE**:
  - `self.current_reason` is `None` or not in
    `{RateLimit, QuotaExceeded, AwaitingOperator}` (race mutex not held)
  - `silence_exceeds_threshold` is `true` (threshold varies by
    `AgentState`: 120s default; 600s for `Thinking | ToolUse`; never for
    `Idle`; 120s for `Starting`)
  - `input_pending_past_response` is `true`:
    `last_input_at_ms > last_heartbeat_at_ms + INPUT_RESPONSE_GRACE_MS`
    (grace = 5_000 ms)
  - `self.state != HealthState::Hung` (first detection latches the state
    flip; subsequent ticks short-circuit)
- **POST**:
  - `self.state = HealthState::Hung`
  - `check_hang` returns `true` (only on the first detection — caller
    escalates)
  - `tracing::warn!` with structured fields
    `last_input_at_ms / last_heartbeat_at_ms / input_response_delta_ms / silent_secs / agent_state`
- **FP vector** — Operator typed input that incremented `last_input_at_ms`
  but the agent is genuinely producing keystrokes that drain through MCP
  without flushing visible PTY output. Bounded by heartbeat semantics:
  any MCP tool call refreshes `last_heartbeat_at_ms` and pulls the
  delta back below the 5s grace.
- **FN vector** — F9 grey failure: an agent producing 1-byte output
  (spinner / log line / partial token) resets the upstream silence
  timer in `StateTracker`, so `silent` never crosses the threshold even
  if no useful work is happening. Productive-output detection is the
  F9 sub-task; this audit only records the gap.

### §Entry.E2 — heartbeat fresh but PTY silent (F1 cross-check)

- **Find in source**: `rg "Hung Entry \(E2\)" src/health.rs`
- **PRE**:
  - `self.current_reason` race mutex same as §Entry.E1
  - `silence_exceeds_threshold` is `true` (same thresholds as §Entry.E1)
  - `input_pending_past_response` is `false` (no input pending; §Entry.E1 did not fire)
  - `heartbeat_fresh` is `true`: `last_heartbeat_at_ms > 0` AND
    `heartbeat_age_ms < silent.as_millis()` — i.e. the agent has called
    MCP tools recently (refreshing heartbeat) while producing no PTY
    output
  - `self.state != HealthState::Hung`
- **POST**:
  - `self.state = HealthState::Hung`
  - `check_hang` returns `true`
  - `tracing::warn!` with structured fields
    `last_heartbeat_at_ms / heartbeat_age_ms / silent_ms / agent_state`
- **FP vector** — F39: stale `AgentState::Thinking` pattern in vterm
  scrollback (the regex match is against rendered screen text and can
  latch on text that scrolled off-screen). Bounded by
  `LATCHED_STATE_EXPIRY` (30s) in `src/state.rs` but not perfectly. See
  §F39 cross-reference.
- **FN vector** — F9 same as §Entry.E1; sub-threshold output keeps
  `silent` below trigger.

## Exit transitions

### §Exit.X1 — silence drops below threshold (recovery)

- **Find in source**: `rg "Hung Exit \(X1\)" src/health.rs`
- **PRE**:
  - `self.state in {HealthState::Hung, HealthState::IdleLong}` (shared
    predicate; one mutation site serves both states)
  - `!silence_exceeds_threshold` (any output, including a single byte,
    drops `silent` below the per-`AgentState` threshold)
- **POST**:
  - `self.state = HealthState::Healthy`
  - `check_hang` returns `false`
- **FP vector — F10 tangential concern** — There is no productive-work
  evidence requirement. **A single byte of PTY output flips Hung to
  Healthy**, even if it is a TTY spinner tick rather than progress. F10
  sub-task is a doc-only confirmation; F9 sub-task is the productive-
  output gate that would tighten this exit predicate.
- **FN vector** — None directly; this is the recovery path. Indirect:
  if §Exit.X1 fires spuriously (F10), the operator may dismiss a
  genuinely stuck agent on the basis of a stale "Healthy" classification.

## IdleLong transitions

`IdleLong` exists to distinguish "agent silent because no one is asking
it anything" from "agent silent because it stopped responding to input"
(Hung). The 04:00 UTC false-alarm pattern motivated the split.

### §IdleLong.Entry.E1 — silent past threshold, no input pending

- **Find in source**: `rg "IdleLong Entry \(E1\)" src/health.rs`
- **PRE**:
  - `self.current_reason` race mutex same as §Entry.E1
  - `silence_exceeds_threshold` is `true`
  - `input_pending_past_response` is `false` (no input pending past heartbeat)
  - `heartbeat_fresh` is `false` (heartbeat older than silent duration)
  - `self.state != HealthState::IdleLong`
- **POST**:
  - `self.state = HealthState::IdleLong`
  - `check_hang` returns `false` (escalation consumers act only on `Hung`
    per the rustdoc contract at `rg "Returns .true. ONLY when transitioning" src/health.rs`)
  - `tracing::debug!` (not `warn!` — non-escalation)
- **FP vector** — Genuinely idle agent waiting for the next operator
  prompt; classification is correct.
- **FN vector** — F9: same shape as §Entry.E1 / §Entry.E2.

### §IdleLong.Exit.X1 — shared with §Exit.X1

- **Find in source**: same `rg "Hung Exit \(X1\)" src/health.rs` (the
  `matches!(state, Hung | IdleLong)` predicate is one mutation site)
- **PRE**: same as §Exit.X1, but the `state` precondition is
  `HealthState::IdleLong`
- **POST**: same as §Exit.X1 (`state = HealthState::Healthy`,
  `check_hang` returns `false`)
- **FP / FN**: same as §Exit.X1

## Productive-output supplement (F9)

This is the maintained contract for the F9 sub-finding (`#685` Phase 1
deliverables #2 + #3): the dual-path supplement to silence-based Hung
detection. It is a companion to the F9 inline structured comments in
`src/state/mod.rs`, `src/behavioral.rs`, and `src/health.rs`.

**Current baseline**: revalidated at `main@1d83b423` (2026-07-16). The gate
remains shadow-by-default (`AGEND_PRODUCTIVE_GATE=1` enables classification).
Gemini-specific calibration was renamed to Agy when Gemini retired. Grok is a
supported backend but currently uses the generic marker/cache path; its
backend-specific F9 calibration remains unverified.

Issue: [#685](https://github.com/suzuke/agend-terminal/issues/685) F9
sub-finding. Sibling sub-tasks: 1 (Hung audit, PR #750), 2 (F39 audit, PR
#752), 3 (F39 speculative narrow, PR #763).

Decision chain:
- `d-20260513154400110972-2` (sub-task 1 base — Hung invariants)
- `d-20260513161542381785-0` (sub-task 2 audit — F39 hypotheses)
- `d-20260513231713506833-1` (sub-task 3 speculative — F39 Gemini narrow)
- `d-20260513235514013631-0` (sub-task 4 — F9 productive-output gate)

Maintenance: section IDs (`§F9.1`-`§F9.5`) are contract anchors. Renaming
must propagate in the same PR. M1/M2/M3 discipline from the Hung audit applies:
inline comments use `§F9.<n>` refs, this doc uses `rg <pattern>` grep hints, and
section headings are stable.

### §F9.1 — Architecture rationale (dual-path supplement)

The naïve framing "replace silence_exceeds_threshold with productive-output
detection" is **wrong**. It would regress `#659` silent-stuck-in-thinking
detection — a process that truly stops emitting any PTY bytes would never
trigger Hung if classification waited for *productive* bytes specifically
(none come; agent stays in pre-Hung state forever).

F9 ships as a **dual-path supplement**:

- **Existing silent path** (any output = alive, threshold-based) remains in
  `check_hang`. It catches the `#659` case where the agent goes truly silent.
- **Productive path** supplements it: when silence is below threshold (the
  agent is producing *some* output) but no *productive* output has arrived
  past a separate threshold, the agent is flagged as a Hung candidate. This
  catches the F9 grey failure where 1-byte spinner output resets the upstream
  silence timer indefinitely while no real work happens.
- `check_hang` returns `true` if EITHER path triggers; the union strictly ADDS
  coverage and never removes it.

This is the layer at which F9 operates: **HealthState** classification. The
sibling F39(c) hypothesis (§F39.4) operates at the **AgentState** layer
(`Thinking` pattern stickiness) — a different concern and bug surface.

### §F9.2 — Productive-signal design

A signal is **productive** if either:

1. **MCP heartbeat** refreshed recently (`use_heartbeat: true` configs).
   Heartbeat refresh means the agent called an MCP tool, concrete evidence of
   forward progress. This is universal across managed backends.
2. **Structural marker** matched the rendered screen text. Markers use
   **line-start anchors and specific formats** — NOT bare keywords. The
   bare-keyword approach (`Saved` / `Wrote`) has the same scrollback FP surface
   as the F39 audit's Scenario A/B/C taxonomy.

Generic structural anchors shared by all backends (file save banners):
- `^Saved to \S+` — file save banner
- `^Wrote \d+ bytes` — explicit byte count
- `^Created file: \S+` — structured creation

Per-backend completion markers shipped in `#685` sub-task 6 (deliverable #4,
decision `d-20260514022917793418-0`). Each backend's `MARKERS` const lists the
generic anchors above plus its own completion-glyph + tool-vocabulary regex.
Look up each via `rg "<BACKEND>_PRODUCTIVE_MARKERS" src/behavioral.rs`:

| Backend | Completion regex (added to generic anchors) | Source | Validation |
|---|---|---|---|
| Claude | `^[✓●⏺]\s+(Read\|Bash\|Edit\|Write\|Grep\|Glob\|Listing\|Reading\|Writing\|Searching\|Editing)\b` | `CLAUDE_PRODUCTIVE_MARKERS` in `src/behavioral.rs` | F685 fixture `f685-f9-positive-savedfile.raw` (synthetic). Real captures pending corpus growth. |
| Kiro | `^●\s+(Read\|Write\|Edit\|Bash\|Grep\|Glob\|Task\|List\|Search)\b` PLUS `\[(fs_read\|fs_write\|execute_bash)\]` | `KIRO_PRODUCTIVE_MARKERS` in `src/behavioral.rs` | Synthetic unit tests only — `kiro_markers_*` in `src/behavioral.rs` tests. **Not validated against real captures** — use the fixture capture playbook. |
| Codex | `^•\s+(Explored\|Edited\|Ran)\b` PLUS `apply_patch` | `CODEX_PRODUCTIVE_MARKERS` in `src/behavioral.rs` | **Synthetic only — not validated against real captures.** Use the fixture capture playbook. |
| Agy | `^✓\s+(ReadFile\|WriteFile\|ReadManyFiles\|Edit\|Shell\|WebFetch\|Glob\|GoogleSearch\|MemoryTool\|ReadFolder)\b` | `AGY_PRODUCTIVE_MARKERS` in `src/behavioral.rs` | Inherited from the retired Gemini engine calibration; current Agy real-capture validation remains incomplete. |
| OpenCode | `^→\s+(Read\|Write\|Edit\|Glob\|Grep\|Bash\|List\|Task)\b` | `OPENCODE_PRODUCTIVE_MARKERS` in `src/behavioral.rs` | **Synthetic only — not validated against real captures.** Use the fixture capture playbook. |
| Grok | Generic save-banner anchors only | `GENERIC_PRODUCTIVE_MARKERS` via `grok_profile()` in `src/backend_profile.rs` | **Unverified backend-specific coverage**; no Grok-labelled corpus fixture. |

**Excluded from F9 markers**:
- All in-progress / spinner glyphs (Braille `[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]`, OpenCode
  `✱`, Codex `◦ Working`, historical Gemini `⠦ Thinking`). F9 productivity =
  completion-only; these fire BEFORE work completes.
- Agy/Gemini-engine `tool.*call` / `MCP.*tool` literals — the heartbeat path
  already covers MCP signals, so counting them again would duplicate evidence.
- Bare keyword markers (for example, `Saved` / `Wrote` without a line-start
  anchor). Prose such as "I saved your time" must not match. This is pinned by
  `infer_productivity_rejects_bare_keyword_scrollback` (legacy) and
  `<backend>_markers_reject_*` per-backend tests.

#### Cache routing

Per-backend markers are NOT routed via pointer equality. That would fall
through to per-call `Regex::new()` compilation — the bug that caused PR #766's
Ubuntu/Windows CI failure. Sub-task 6 introduced the `MarkerCacheId` enum on
`ProductivityConfig`:

```rust
pub enum MarkerCacheId { Generic, Claude, Kiro, Codex, Agy, OpenCode }

pub struct ProductivityConfig {
    pub markers: &'static [&'static str],
    pub use_heartbeat: bool,
    pub heartbeat_fresh_window_ms: u64,
    pub cache_id: Option<MarkerCacheId>,
}
```

`infer_productivity` matches on `cache_id` to route to the corresponding
per-backend `LazyLock<Vec<Regex>>` static (`CLAUDE_PRODUCTIVE_REGEXES`, etc.).
The compile-time exhaustive match prevents missing-backend bugs. `None` is
reserved for ad-hoc test configs and falls back to `Regex::new()` per call;
Phase 1 production code never reaches that path.

Future per-backend `heartbeat_fresh_window_ms` tuning and per-backend silence
threshold tuning are out of scope until corpus measurement data supports the
calibration (see §F9.5).

### §F9.3 — Dual-path decision table

| `silent` | `silent_productive` | `agent_state` | Default mode | `AGEND_PRODUCTIVE_GATE=1` |
|---|---|---|---|---|
| ≤ threshold | ≤ threshold | any | not-Hung | not-Hung |
| > threshold | any | non-Idle | discriminator (existing) | discriminator (existing) |
| ≤ threshold | > threshold | non-Idle | not-Hung + telemetry | discriminator (NEW path) |
| any | any | `Idle` | not-Hung (Idle never hangs) | not-Hung (Idle never hangs) |

"discriminator" means the existing input-pending-past-heartbeat /
heartbeat-fresh / IdleLong branch in `check_hang`. Both paths route into the
same discriminator once a path triggers. F9 adds a new entry condition, not a
new discriminator branch.

Find threshold mapping in source with
`rg "silence_exceeds_threshold" src/health.rs` (silent path) and
`rg "productive_silence_exceeds" src/health.rs` (F9 path). Both currently use
the same per-`AgentState` thresholds.

### §F9.4 — Known limitations (to be measured by fixture corpus)

#### 4.1 Heartbeat-as-productive gap

Pure-reasoning long sessions (for example, Claude internal thinking without MCP
tool calls) generate no heartbeat refresh **and** no productive markers. Once
`AGEND_PRODUCTIVE_GATE=1` is set, these sessions are flagged Hung after the
threshold despite legitimate work in progress.

**Mitigation deferred**:
- A follow-up integrating spinner-cycling-as-productive (the F39 hypothesis (e)
  variant — pattern-source-line tracking, inverted to count spinner-glyph
  activity as evidence).
- An operator override mechanism (out of F9 scope).

**Risk-contained**: shadow mode plus env-var opt-in means only opted-in users
encounter this FN during rollout. Activation is gated on fixture-corpus
measurement (§F9.5).

#### 4.2 Generic markers FP residual

Even with structural anchors, edge cases remain. A user pasting the literal
string `Saved to /tmp/foo` into a chat message that the agent echoes could match
the marker. The negative test `infer_productivity_rejects_bare_keyword_scrollback`
pins the anchor approach, which substantially narrows but does not eliminate the
surface.

**Mitigation applied**: line-start anchors (`^`) plus specific formats. Tests
pin both the positive (real markers match) and negative (bare prose does not)
contracts.

**Residual risk acknowledged**: keep measuring it with real captured fixtures.

#### 4.3 Cross-backend pattern uniformity

Phase 1 shipped the same generic markers across all backends. Later calibration
added Kiro `[fs_read]` and Agy tool banners (renamed from the retired Gemini
engine). Grok still uses the generic profile, so backend-specific Grok progress
that differs from those anchors has lower F9 sensitivity until real-capture
calibration lands.

### §F9.5 — Activation gate (shadow → opt-in → promotion)

F9 productive-silence telemetry fires unconditionally
(`rg "F9 dual-path candidate" src/health.rs`). **Classification** is gated on
the `AGEND_PRODUCTIVE_GATE` env var:

```
unset / not "1"   → shadow mode (default): telemetry collected,
                    no Hung classification from productive path
"1"               → active mode: productive-silence path can flag Hung
```

**Anti-dead-infra clause**: the Sprint 27 PR-A behavioral telemetry shipped in
shadow mode and never promoted. The productive-output path therefore keeps
explicit promotion criteria:

1. **Fixture corpus measures FP rate < 1% with N ≥ 300 not-stuck fixtures**
   (the Rule-of-Three statistical minimum; the original `#685` wording used a
   smaller 3+ case floor). Capture and grow the corpus with the
   [PTY fixture capture playbook](../tests/fixtures/state-replay/CAPTURE-RECIPES.md),
   and run the measurement in `tests/fixture_corpus_measurement.rs` with
   per-transition, source-separated reporting.
2. **2-week shadow-mode telemetry shows behavioral divergence stable**
   (the Sprint 27 PR-A divergence-dashboard pattern, found with
   `rg "behavioral_shadow" src/behavioral.rs`).
3. **Operator decision to flip the env-var default**, in a separate PR that
   changes the default in `check_hang` from unset to `"1"`, or removes the gate
   entirely once promoted.

Without all three, the gate stays default-off. If the timeline exceeds 6 weeks
without measurement, the F9 path itself becomes a removal candidate; dead
shadow infrastructure is worse than no infrastructure.

#### 5.1 Cross-references

- §F39.4 hypothesis (c) footnote — layer distinction between F39(c)
  `AgentState` and F9 `HealthState`.
- §Invariants 5b/5c — F9 preserves the `check_hang -> bool` return contract and
  `display_name()` string contract. The new `silent_productive: Duration`
  parameter is an internal-API refactor; the sole production caller is
  `src/daemon/per_tick/hang_detection.rs` (`rg "check_hang(" src/daemon`).
- `src/behavioral.rs` — `ProductivitySignal`, `ProductivitySource`,
  `ProductivityConfig`, `config_for_productivity`, `infer_productivity`, and
  `log_productivity_telemetry`. Historically these paralleled the Sprint 27
  PR-A silence-side equivalents (`BehavioralSignal` / `infer_from_silence` /
  `log_shadow_telemetry`); those were removed in #2547 as dead code, while
  `BehavioralConfig` remains because `backend_profile.rs` still uses it.
- [RECOVERY-STAGES.md](RECOVERY-STAGES.md) — the current Stage-1-only recovery
  dispatcher reads `productive_silence_exceeds` directly to select the Stage 1
  alive-stuck versus dead-likely branch. Recovery treats all `Hung` sources the
  same regardless of F9 promotion state; see §RS.4.

## §F39 — AgentState Thinking Pattern Stickiness (cross-audit: AgentState, not HealthState)

This section is a **cross-audit boundary**: §F39 documents `AgentState::Thinking`
pattern semantics in `src/state.rs`, which feed `check_hang` as an input
signal but are not themselves `HealthState` mutators. F39 is included in
this Hung-state audit because the `AgentState::Thinking` pattern feeds the
§Entry.E2 precondition path (heartbeat-fresh + PTY-silent classification),
so stale-pattern false positives propagate into Hung detection.

Scope: pattern stickiness audit, possible mitigations as **hypotheses
only** (no FP-rate data available — fixture-corpus validation pending
sub-task `#685` deliverable 5). Implementation of any mitigation is
strictly out of scope.

Sibling decision: `d-20260513161542381785-0` (sub-task 2 of N).

### §F39.1 — Patterns per backend

`AgentState::Thinking` is matched per-backend via regex pattern catalogs in
`src/state.rs`. Patterns are scoped to a single backend (state pattern
lookup keyed on `Backend` enum variant during `StateTracker::new`), so
cross-backend contamination requires the prior step (backend detection)
to be wrong — see §F39.5 cross-backend overlap.

| Backend | Pattern | Find in source | Source evidence | History |
|---|---|---|---|---|
| Kiro (kiro-cli) | `r"Kiro is working\|esc to cancel"` | `rg "Kiro is working" src/state.rs` | `[measured]` comment above pattern line | Sprint 34 PR-1 (`Kiro is working` shown during generation) |
| Gemini (gemini-cli) | `r"esc to cancel"` | `rg "esc to cancel" src/state.rs` | `[measured]` comment near pattern | Originally bare `r"Thinking"` — already narrowed to `esc to cancel` to reduce stale matches. Further narrow (e.g. require leading Braille spinner `⠦`) is a candidate quick-win to-be-evaluated in a separate follow-up PR, NOT in this audit. |

Cross-backend overlap: the literal `"esc to cancel"` substring appears in
both Kiro and Gemini patterns. Because pattern catalogs are scoped
per-backend, this is benign **as long as backend detection is correct**.
If `Backend::from_command` mis-routes (e.g. unfamiliar binary name), the
catalog used is `None` (Shell/Raw fallback) which has **no Thinking
pattern**, so cross-contamination requires an active mis-route to a
different managed backend. Out of scope for this audit — see §F39.5.

### §F39.2 — LATCHED_STATE_EXPIRY semantics

```rust
const LATCHED_STATE_EXPIRY: Duration = Duration::from_secs(30);  // src/state.rs
```

The expiry interacts with active-state hysteresis via
`maybe_expire_latched_state` (`rg "fn maybe_expire_latched_state" src/state.rs`):
when `current` is a self-expiring active state (`Thinking | ToolUse`) and
`since.elapsed() >= LATCHED_STATE_EXPIRY`, the tracker transitions to
`Ready`. The fallback fires from two call-sites:

1. `feed()` non-match branch (`rg "maybe_expire_latched_state" src/state.rs` —
   first call-site, line near 759) — when screen changed but no pattern
   matched, the fallback drops stale latched state.
2. `tick()` periodic supervisor call (`rg "fn tick" src/state.rs` — second
   call-site, line near 843) — runs even when no PTY output (covers the
   "screen frozen at dismissed prompt" case from prior incident
   `dev-reviewer 卡在互動 prompt`).

Both call-sites depend on `since` having actually elapsed past
LATCHED_STATE_EXPIRY. The Scenario C bug (§F39.3) is that `since` keeps
getting reset by priority oscillation before the threshold can be reached.

### §F39.3 — Scenario taxonomy A/B/C (centerpiece)

The intuitive "scrollback pattern re-matches → `since` resets → expiry
never fires" framing is **wrong**. Two existing guards prevent the naive
re-match path from breaking expiry:

- `feed()` hash-dedup (`rg "last_screen_hash" src/state.rs`) — if the
  rendered screen hash is unchanged, `feed()` short-circuits before
  reaching `detect()`. Same hash ⇒ same patterns visible ⇒ no spurious
  re-detect.
- `transition(same_state)` early return (`rg "if new_state == self.current" src/state.rs`) —
  if `detect()` returns the same state we're already in, `transition()`
  short-circuits without touching `since`.

These two guards correctly handle scenarios A and B. Scenario C is the
actual bug surface.

**Scenario A — pattern in scrollback, screen static (WORKING)**

The agent is `Thinking`. The active spinner stops rendering but `esc to
cancel` text remains visible on a frozen screen. The screen hash is
unchanged across ticks, so `feed()` short-circuits at the hash-dedup
gate. `detect()` is not called; `since` stays at the original
transition timestamp. After `LATCHED_STATE_EXPIRY` elapses, `tick()`
fires `maybe_expire_latched_state` → transition to `Ready`. **No bug.**

Footnote — screen resize: a terminal resize forces vterm buffer realloc,
which changes the screen hash even without semantic change. This
re-triggers `detect()`, but if the pattern still matches (same
text content, different layout), the result is Scenario B — also handled
correctly. A resize without pattern-text change is Scenario A unchanged.

**Scenario B — screen changes, state pattern unchanged (WORKING)**

The agent is `Thinking`. New content scrolls in but `esc to cancel`
remains visible. Hash changes, `detect()` runs and returns `Thinking`,
but `transition(Thinking)` early-returns because `new_state == self.current`.
`since` is unchanged. `tick()` eventually fires expiry. **No bug.**

**Scenario C — priority oscillation under conflicting patterns (BROKEN)**

Sequence (numbers are illustrative; behaviour holds for any oscillating
priority pair):

```
t=0s   agent enters Thinking (priority 6); since=0
t=10s  spinner clears, shell prompt `❯` becomes visible
       detect() returns Idle (priority 4)
       transition(Idle): priority-down + held >= 2s active min_hold
         → state=Idle; since=10s
t=15s  screen scrolls; `esc to cancel` re-enters viewport
       detect() returns Thinking (priority 6)
       transition(Thinking): higher priority always wins, instant
         → state=Thinking; since=15s   ← `since` reset by bounce
t=25s  agent action clears spinner; `❯` again
       transition(Idle) → state=Idle; since=25s
t=40s  scroll triggers `esc to cancel` re-detection
       transition(Thinking) → state=Thinking; since=40s
...
```

Each different-state transition resets `since`. The 30s
`LATCHED_STATE_EXPIRY` predicate `since.elapsed() >= 30s` never holds
long enough to fire because successive bounces keep `since` recent. The
agent appears Thinking indefinitely to upstream consumers (including
`check_hang`'s §Entry.E2 path).

**Precise mechanic**: priority oscillation resets `since` per bounce.
(Note: the doc deliberately does not use "afterglow" language — that
implies a decaying signal, but the actual bug is `since=now` reset, not
decay.)

### §F39.4 — Possible mitigations to-be-validated

**No FP-rate data available. These are hypotheses for fixture corpus
validation (`#685` sub-task 5). Not recommendations.**

| Hypothesis | Description | Measurement required |
|---|---|---|
| (a) Cursor-anchored / viewport-only | Match patterns only against last N rows or visible viewport; exclude scrollback rows entirely | Count Scenario C bounces before/after on corpus; **feasibility check**: portable-pty / vterm cursor-position API surface on macOS / Linux / Windows ConPTY (Open question §F39.5) |
| (b) Recent-output-bytes gate | Match against bytes received in last K `feed()` calls (slice accumulated buffer) instead of full rendered screen | Measure output rate distribution per backend; choose K such that legitimate Thinking matches stay above threshold |
| (c) Co-required negative pattern¹ | `Thinking` valid only if `esc to cancel` present AND prompt indicator (e.g. `❯`) absent | Count Thinking→Idle transitions with spinner visible on corpus |
| (d) Oscillation-detection min-hold extension | Counter detects ≥2 transitions touching the same state within N seconds → extend `min_hold` to N × K seconds before allowing further transitions | Measure oscillation frequency on corpus |
| (e) Pattern-source-line tracking | `detect()` returns match row index; scrollback rows (above viewport top) yield "stale" verdict and skip `transition()` | Measure scrollback-vs-viewport match rate per pattern |
| (f) Per-pattern / dynamic `LATCHED_STATE_EXPIRY` | Per-pattern expiry value (shorter for `Thinking`), or dynamic shrink when current state held > 2× typical duration | Measure typical Thinking duration per backend; identify outliers |

**Distinct levers — (d) vs (f)**: (d) extends `min_hold` (the priority
transition gate at `rg "min_hold" src/state.rs`) to make oscillation
harder; (f) shortens `LATCHED_STATE_EXPIRY` so the latched state
expires sooner. Both are independently composable.

¹ Hypothesis (c) variant — narrowing Gemini Thinking pattern from
`r"esc to cancel"` to `r"\(esc to cancel,"` — speculatively applied in
PR #763 (decision `d-20260513231713506833-1`). Reduces FPs from stale
`esc to cancel` text in scrollback without requiring co-pattern gating.
Re-evaluate the full (c) hypothesis when fixture corpus data is available.

**F9 layer distinction**: this hypothesis lives at the `AgentState`
layer (`Thinking` pattern stickiness in `src/state.rs`). The F9
productive-output gate (§F9.1–§F9.5, decision
`d-20260513235514013631-0`) operates at the `HealthState`
layer (`Hung` classification in `src/health.rs::check_hang`). The two
do NOT overlap: F39 mitigations adjust *which `AgentState::Thinking`
transitions fire*, while F9 adds a parallel `HealthState::Hung`
classification path independent of `AgentState`. A fix at one layer
does not subsume a fix at the other.

**Rejected**: tick force-recheck on screen-hash change — `tick()` already
calls `maybe_expire_latched_state` periodically (`rg "fn tick" src/state.rs`),
and the underlying `since.elapsed() >= LATCHED_STATE_EXPIRY` check is
identical regardless of caller. Does not address Scenario C's `since`
reset mechanic.

### §F39.5 — Open questions

- **F9 / F39 interaction warning**: F9 productive-output signal
  (separate sub-task) will inherit Scenario A/B/C surface if it uses
  PTY pattern matching as evidence. F9 sub-task design must consider
  scrollback-staleness from day-1; the same A/B/C taxonomy applies.

- **Fixture corpus Scenario C capture acceptance criteria**: the
  fixture corpus sub-task (`#685` deliverable 5) must include traces
  where `AgentState` alternates between `Thinking` and a non-`Thinking`
  state (`Idle`, `Ready`, etc.) **≥3 times within a 30-second window**,
  with `esc to cancel` (or other Thinking-pattern substring) visible in
  scrollback throughout. Without Scenario-C-specific capture, FP-rate
  measurement of hypotheses (a)–(f) cannot differentiate fixes that
  address the bug from fixes that only mask Scenarios A and B.

  **Update (sub-task 5 ship)**: corpus infrastructure landed. The Scenario C
  measurement itself remains deferred — replay tests run in microseconds, so
  wall-clock-based `min_hold` thresholds never cross during byte-only replay.
  A time-injection harness extension remains required; collect new traces with
  the [PTY fixture capture playbook](../tests/fixtures/state-replay/CAPTURE-RECIPES.md).

- **Cross-backend pattern overlap**: Kiro `r"Kiro is working|esc to cancel"`
  and Gemini `r"esc to cancel"` share the literal `"esc to cancel"`. State
  patterns are per-backend (`StateTracker::new(Some(&backend))` → backend-
  specific catalog), so this is benign as long as `Backend::from_command`
  routing is correct. If a backend mis-detect routes the wrong catalog,
  silent latching to the wrong state can occur. Out-of-scope verification —
  worth noting for F9 / mitigation design.

- **Missing unit test for Scenario C**: `rg "oscillation|bounce" src/state.rs`
  → 0 hits in tests. Existing tests cover happy-path
  `LATCHED_STATE_EXPIRY` (`rg "fn feed_fallback_expires_thinking" src/state.rs`),
  but not priority oscillation. Add Scenario-C-specific unit test when
  any mitigation sub-task lands.

- **Cursor-anchored feasibility check**: hypothesis (a) depends on
  cursor-position API surface in `portable-pty` and the `VTerm`
  abstraction on each supported platform (macOS, Linux, Windows
  ConPTY). Verify availability before any implementation PR — if
  cursor position is not reliably exposed on Windows ConPTY,
  hypothesis (a) is infeasible cross-platform.

## F9 / F10 follow-up scope cross-reference

The F9 row is now maintained by §F9.1–§F9.5 in this contract. The table remains
as the transition-to-finding map.

| Finding | Affected transitions | Sub-task scope |
|---|---|---|
| F9 (productive-output gate) | §Entry.E1 FN, §Entry.E2 FN, §IdleLong.Entry.E1 FN, §Exit.X1 FP | Dual-path productive-output signal and activation gate maintained in §F9.1–§F9.5; changes remain internal to `StateTracker` and/or `check_hang` predicates. |
| F10 (doc-only confirmation) | §Exit.X1 FP | Confirm that `maybe_decay` truly does not affect Hung (this audit's §Invariants 5b/5d is the evidence) and that §Exit.X1 is the sole recovery path. Doc-only sub-task. |

## Open questions (for Phase 2 / future sub-tasks)

- **ErrorLoop entry without exit** — `rg "HealthState::ErrorLoop" src/health.rs`
  returns one entry site (in `record_error`) but no observed
  `HealthState::ErrorLoop → Healthy` exit transition. Separate audit
  warranted; out of scope for Hung audit.
- **Fixture corpus design** — Phase 1 deliverable #5 (replay captured
  stuck-thinking incidents from #659 and others) is a separate
  sub-task. Acceptance criterion: FP < 1% / FN < 10% per the issue.
- **Backend-specific tuning hooks** — Phase 1 deliverable #4
  (kiro/gemini may need different thresholds than claude); separate
  sub-task.
- **Stage-1 / Stage-2 / Stage-3 recovery design** — Phase 2 of #685,
  gated behind feature flags and operator default of "warn-only" per
  the issue. **Update (sub-task 7a ship)**: Stage 1 ESC interrupt
  infrastructure shipped — `src/daemon/per_tick/recovery_dispatcher.rs`
  + `RecoveryStageState` state machine + `HealthState::Paused` variant
  + env-var-gated shadow-mode default. Stages 2 and 3 follow-up
  sub-tasks reuse the same dispatcher tick + state machine. See
  [RECOVERY-STAGES.md](RECOVERY-STAGES.md) for the full lifecycle and
  promotion criteria.
  **Update (#2549)**: Stage 2/3 (and the dispatcher-driven Stage 3 arm)
  were later removed — converged to Stage-1-only. See
  [RECOVERY-STAGES.md](RECOVERY-STAGES.md)'s header banner for the rationale.

## Consumer audit

At the original `2f24376` audit baseline, §Invariants 5c recorded the surface
below. Current consumers are listed first so the historical conclusion cannot
be mistaken for live behavior:

- **Current transition consumer**: `HangDetectionHandler::run` calls
  `check_hang` and logs the transition; for qualifying self-orchestrators it
  also persists Hung-entry/exit escalation anchors
  (`src/daemon/per_tick/hang_detection.rs`).
- **Current state consumer**: `RecoveryDispatcherHandler::run` reads
  `core.health.state == Hung` on the same and later ticks. It can issue the
  Stage-1 ESC only when `AGEND_AUTO_RECOVERY_STAGE1=1`; default shadow mode
  logs the decision without PTY I/O. `RespawnWatchdogHandler` owns a separate
  resume-spawn failure path and can enter `Paused`.
- **Display projection**: `health.state.display_name()` is serialized for API,
  MCP, snapshot, and UI consumers. Treat the string as a projection, not a
  mutation authority.

The old grep result (zero `HealthState::Hung` consumers outside
`src/health.rs` at `2f24376`) is retained only as the **pre-recovery baseline**;
it is expected to be false on current source.
