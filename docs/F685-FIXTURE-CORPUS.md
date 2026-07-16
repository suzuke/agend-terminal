# F685 Fixture Corpus

Source-of-truth for the F685 fixture corpus and measurement harness.
Unlocks F39 mitigation selection (six hypotheses fixture-FP-gated, see
`docs/HUNG-STATE-TRANSITIONS.md §F39.4`) and F9 promotion-to-default-active
(FP < 1% with the statistical minimum in §F685-CORPUS.4, see
`docs/F9-PRODUCTIVE-OUTPUT-GATE.md §F9.5`).

> **CURRENT CORPUS (revalidated at `main@1d83b423`, 2026-07-16):**
> `tests/fixtures/state-replay/MANIFEST.yaml` is authoritative and currently
> contains **44 fixtures**, including **8 schema-v2-labelled fixtures**:
> six `silent_stuck`, one `productive_marker_fire`, and one
> `productive_silence`. The manifest covers Agy, Claude, Codex, Kiro/Kiro
> CLI, and OpenCode; it has **no Grok fixture yet**. Gemini is retired and no
> longer appears in the live manifest. Section §F685-CORPUS.3 preserves the
> smaller launch corpus as historical provenance, not as a current count.

Decision: `d-20260514015214320625-1` (sub-task 5 of N for `#685`).
Sibling chain: sub-tasks 1 (Hung audit, PR #750), 2 (F39 audit, PR #752),
3 (Gemini regex narrow, PR #763), 4 (F9 productive-output gate, PR #766).

Maintenance: section IDs (`§F685-CORPUS.1`-`§F685-CORPUS.7`) are stable
contract anchors. M1/M2/M3 discipline from sub-task 1 applies (inline
comments cross-ref `§F685-CORPUS.<n>`; this doc uses `rg <pattern>` hints
for source refs).

## §F685-CORPUS.1 — Purpose & cross-cutting nature

The corpus is **shared infrastructure** across multiple `#685` deliverables:

- **F9 promotion gate** — `check_hang` productive-silence path classification
  measured against `expected_hung_classification` ground truth. Promotion
  criteria require FP < 1% on N ≥ 300 not-stuck fixtures (statistical Rule
  of Three at 95% confidence) + 2-week shadow telemetry stable.
- **F39 mitigation selection** — six hypotheses (a)/(b)/(c)/(d)/(e)/(f) in
  `docs/HUNG-STATE-TRANSITIONS.md §F39.4` need FP measurement to pick a
  winner. Same corpus, different harness pass (per `§F685-CORPUS.4`).
- **Recovery calibration** — the live Stage-1 recovery action remains
  shadow-by-default and needs confidence in detection FP/FN before promotion.

The corpus does **not** belong exclusively to F9 or F39 — it is the
**measurement substrate** both rely on. Hence its standalone doc and
top-level integration test entry point.

## §F685-CORPUS.2 — Manifest schema extension

`ReplayFixture` at `rg "struct ReplayFixture" src/state/tests.rs` was extended
with seven optional fields (serde defaults preserve backward-compat with
schema-v1 fixtures):

| Field | Type | Purpose |
|---|---|---|
| `scenario_kind` | `Option<String>` | One of: `scrollback_static`, `screen_change_same_state`, `priority_oscillation`, `productive_marker_fire`, `productive_silence`, `silent_stuck`, `productive_bursty`. Drives harness measurement dispatch. |
| `expected_hung_classification` | `Option<String>` | Ground truth for F9 promotion measurement. One of: `not_hung`, `hung`, `ambiguous`. |
| `expected_oscillation_count` | `Option<u32>` | F39 measurement: how many priority transitions the trace should produce when wall-clock-injection is enabled (deferred — `§F685-CORPUS.6`). |
| `productive_marker_expectations` | `Vec<{time_ms, source}>` | F9 detailed measurement: which markers fire at which times. Default empty for fixtures without expectation. |
| `capture_kind` | `Option<String>` | One of: `real`, `synthetic`, `synthetic_from_real_template`. Drives source-separated reporting per `§F685-CORPUS.4`. |
| `provenance` | `Option<String>` | Human-readable origin: PR #, operator session note, or `synthetic from <template>`. Audit trail. |
| `schema_version` | `u32` (default `1`) | Future-compat marker. **No runtime enforcement in Phase 1** — informational only; future schema changes bump and add migration. |

**Backward-compat**: schema-v1 fixtures parse unchanged via serde defaults.
The `state::tests::replay_manifest_regression` test pins the compatibility
path.

## §F685-CORPUS.3 — Initial corpus (historical launch snapshot)

> The counts and backend names in this section describe the Phase 1 launch
> plan. They are retained to explain the original measurement design. Use
> the current-corpus banner above and `MANIFEST.yaml` for live coverage.

The Phase 1 documentation listed three synthetic schema-v2 fixtures plus the
then-current schema-v1 baseline:

| Fixture | Backend | Scenario | Classification | Capture |
|---|---|---|---|---|
| `f685-f9-positive-savedfile.raw` | claude-code | `productive_marker_fire` | `not_hung` | `synthetic` |
| `f685-f9-negative-saved-prose.raw` | claude-code | `productive_silence` | `not_hung` | `synthetic` |
| `f685-silent-stuck-stub.raw` | gemini | `silent_stuck` | `hung` | `synthetic_from_real_template` (historical planned stub; not in the current manifest) |

At launch, 13 legacy schema-v1 fixtures (1 per backend × {thinking, tooluse,
+occasional perm/update}) parsed without manifest edits under
`replay_manifest_regression`.

The launch coverage priority was **Gemini + Kiro** (issue `#659` named these
explicitly as known-stuck backends). Gemini was later retired in favour of
Agy. The current corpus adds Agy coverage, while Grok remains the active
backend with no labelled or schema-v1 fixture.

This initial set is **not** statistically sufficient for the FP < 1% /
FN < 10% gates. Corpus growth is delegated to operators and follow-up
sub-tasks per `§F685-CORPUS.6`.

## §F685-CORPUS.4 — Measurement methodology

### Two pipelines on shared corpus

1. **F9 productive-signal pipeline** (active in this PR).
   - Replay each schema-v2 fixture through `VTerm` + `infer_productivity`.
   - Compare resulting `ProductivitySignal` to `scenario_kind` expectation:
     - `productive_marker_fire` → expect `Productive { source: Marker(_) }` or `Productive { source: Heartbeat }`
     - `productive_silence` → expect `NoSignal`
     - `silent_stuck` → expect `NoSignal`
   - Smoke-test pinned in `rg "corpus_measurement_smoke_f9_marker_signals" src/state/tests.rs`.
2. **F39 oscillation pipeline** (deferred, see `§F685-CORPUS.6`).
   - Replay each schema-v2 fixture through `StateTracker::feed` with
     wall-clock injection between chunks.
   - Compare observed transition count to `expected_oscillation_count`.
   - Requires harness extension that backdates `since` per chunk timing
     metadata in the manifest. Not in Phase 1 scope.

### Per-transition unit (NOT per-tick)

A single false-Hung transition × 100 ticks counts as **1** FP event. The
classifier returns `true` only on the entry transition (sub-task 1
§Invariants 5a/5b); harness aggregation mirrors this.

### Source separation in reporting

Reports break out into three lines:

```
F9 measurement (N at report time):
  Real:        X/Y  (high signal value — actual operator sessions)
  Synthetic:   X/Y  (specific scenario coverage — crafted to exercise paths)
  Combined:    X/Y  (aggregate)
```

A synthetic FAIL is immediately actionable (the marker or pattern is
wrong). A real PASS provides ground confidence. A **real FAIL is the
most valuable data point** — it surfaces a production-relevant FP or FN.

The integration test `rg "corpus_count_report" tests/fixture_corpus_measurement.rs`
emits this report via `eprintln!` (visible with `cargo test -- --nocapture`)
and asserts gentle gates on scenario_kind shape (≥1 of each core kind).
Strictness ratchets up as N grows.

### Statistical minimums (delegate-to-growth)

- **FP < 1%** at 95% confidence (Rule of Three): N ≥ 300 not-stuck fixtures
- **FN < 10%** at reasonable confidence: N ≥ 30 known-stuck fixtures

Phase 1 shipped N = 3 schema-v2 fixtures plus the harness; the current
manifest has N = 8 labelled fixtures. **The harness reports rates against
current N**; the promotion gate criteria (in F9
commit msg and F39 audit) gate on `N ≥ minimum AND rate < threshold`.
This is a deliberate reframe of the issue's `FP < 1%` wording from
"hit the bar in one PR" to "hit the bar via corpus growth over time".

### Shadow vs active F9 measurement

- **Shadow mode** (default, `AGEND_PRODUCTIVE_GATE` unset): F9 telemetry
  fires but classification unchanged. Useful for estimating FP rate
  without prod impact.
- **Active mode** (`AGEND_PRODUCTIVE_GATE=1`): F9 actually classifies.
  **Required for promotion-criteria measurement.** Test code uses
  `with_f9_gate(true, || { ... })` from `tests/common/env_gate.rs` (and
  the unit-test mirror in `src/health.rs::tests`).

## §F685-CORPUS.5 — Capture workflow

Reuse the existing protocol documented in `MANIFEST.yaml`:

```sh
script -q /tmp/<backend>-session.raw <cli-command>
# interact: trigger thinking, let it complete, exit
# copy file into tests/fixtures/state-replay/ and add a manifest entry
```

No new CLI tool. Synthetic fixtures use `printf '%b' ...` to write
crafted byte sequences (see git log on the F685 fixtures for examples).

When capturing for F9/F39 measurement, the manifest entry must include
the schema-v2 measurement fields:

```yaml
- file: my-new-capture.raw
  backend: kiro-cli
  cli_version: "X.Y.Z"
  recorded_on: "YYYY-MM-DD"
  scenario: "human-readable summary"
  expected_transitions: [starting, ...]
  expected_final_state: ...
  scenario_kind: silent_stuck  # required for measurement
  expected_hung_classification: hung  # required for measurement
  capture_kind: real  # or synthetic / synthetic_from_real_template
  provenance: "#NNN operator session 2026-..."
  schema_version: 2
```

## §F685-CORPUS.6 — Corpus growth protocol & open questions

### Growth protocol

Corpus grows **incident-driven** over weeks:

1. Operator encounters a stuck-in-thinking or false-Hung incident in
   production.
2. PTY trace captured (or reproduced) via `§F685-CORPUS.5` workflow.
3. Operator (or follow-up sub-task) adds manifest entry with measurement
   labels.
4. Harness re-runs in next CI cycle; aggregate FP/FN report updates.
5. When N reaches statistical minimum AND rates < threshold, promotion
   gate (F9 default-active flip, F39 mitigation pick) unblocks.

Each new fixture is a **follow-up sub-task** of `#685` (not a PR of its
own unless code/harness changes are bundled).

### Open questions

- **Time-injection harness extension**: F39 Scenario C measurement requires
  wall-clock advancement between byte chunks (the priority `min_hold` gates
  in `rg "min_hold" src/state/mod.rs` use `Instant::now()`). The replay loop
  currently runs in microseconds; even a 30-second real trace replays
  instantly, so `since.elapsed()` never crosses `min_hold`. Extension
  needed: per-chunk timestamp metadata in `.raw` companion + harness that
  backdates `since` per chunk. Out of Phase 1 scope; F39 mitigation
  selection blocks on this.
- **Real Scenario C capture**: not yet obtained. Operators encountering
  oscillation should script-capture the session and contribute a real
  fixture; synthetic-from-real-template (timeline-faithful byte sequence
  derived from an operator's incident report) is acceptable in interim.
- **Per-backend marker calibration**: deliverable #4 (sub-task 6,
  decision `d-20260514022917793418-0`) shipped backend-specific marker
  caches, later renamed from Gemini to Agy. Grok currently uses the generic
  cache — see `docs/F9-PRODUCTIVE-OUTPUT-GATE.md §F9.2` for current listings.
  Codex and OpenCode markers remain
  **synthetic-only** pending real PTY captures via this corpus growth
  protocol; corpus-side fixtures for those backends will join the same
  harness loop once captured.
- **Cargo feature gate revisit**: Phase 1 ships always-on harness (zero
  cost when fixtures lack measurement labels). If corpus grows past
  N ≈ 100 fixtures and aggregate replay time approaches CI budget
  (`§F685-CORPUS.7`), reconsider gating the harness behind
  `cargo test --features f9-measure` to keep default `cargo test` fast.

## §F685-CORPUS.7 — Cross-references & boundaries

- `docs/HUNG-STATE-TRANSITIONS.md §F39.5` open questions point here for
  fixture corpus capture criteria.
- `docs/F9-PRODUCTIVE-OUTPUT-GATE.md §F9.5` activation gate points here
  for promotion measurement methodology.
- `src/state/tests.rs::corpus_measurement_smoke_f9_marker_signals` —
  unit-test smoke harness for F9 marker measurement.
- `tests/fixture_corpus_measurement.rs` — integration-test harness for
  manifest schema validation + corpus counts report.
- `tests/common/env_gate.rs::with_f9_gate` — integration-test helper for
  F9 env var serialisation. Mirror copy in
  `src/health.rs::tests::with_f9_gate`; keep in lock-step.
- `tests/fixtures/state-replay/MANIFEST.yaml` — corpus manifest (schema-v1
  baseline + schema-v2 measurement extensions).

### Out of scope (this sub-task)

- F39 mitigation (a)/(b)/(c)/(d)/(e)/(f) selection — needs corpus growth
  + time-injection harness first.
- F9 promotion flip — needs corpus growth + active-mode measurement first.
- Per-backend tuning (deliverable #4) — separate sub-task.
- Recovery automation beyond the current Stage-1-only dispatcher — requires a
  fresh scope decision and new evidence; removed Stages 2/3 are not a live plan.
- Schema migration code for `schema_version` enforcement — Phase 1
  metadata-only.
- `cargo test --features f9-measure` gating — defer until N ≈ 100.
