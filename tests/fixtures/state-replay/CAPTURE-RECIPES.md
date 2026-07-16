[繁體中文](CAPTURE-RECIPES.zh-TW.md)

# PTY Fixture Capture Playbook

Structured recipes for operator-side batch capture of PTY fixtures.
Agents cannot spawn fresh interactive backends (no real PTY allocator),
so all fixture recording must be done by the operator in a live terminal
session. Each `.raw` file records the exact byte stream a backend emits,
including ANSI escapes, cursor positioning, and SGR color codes. These
bytes are the ground truth for state detection, dismiss-pattern matching,
and the composite-signature framework (#996).

`cli_version` matters: patterns detect literal strings that change across
CLI versions. Always record the version and date so regressions can be
distinguished from upstream UI changes.

> **CURRENT CORPUS (revalidated at `main@1d83b423`, 2026-07-16):**
> `MANIFEST.yaml` is authoritative and currently contains **44 fixtures**,
> including **8 schema-v2-labelled fixtures**: six `silent_stuck`, one
> `productive_marker_fire`, and one `productive_silence`. The manifest covers
> Agy, Claude, Codex, Kiro/Kiro CLI, and OpenCode; it has **no Grok fixture
> yet**. Gemini is retired and no longer appears in the live manifest.
> §F685-CORPUS.3 preserves the smaller launch corpus as historical provenance,
> not as a current count.

Decision: `d-20260514015214320625-1` (sub-task 5 of N for `#685`).
Sibling chain: sub-tasks 1 (Hung audit, PR #750), 2 (F39 audit, PR #752),
3 (Gemini regex narrow, PR #763), 4 (F9 productive-output gate, PR #766).

Maintenance: section IDs (`§F685-CORPUS.1`–`§F685-CORPUS.7`) are stable
contract anchors. M1/M2/M3 discipline from sub-task 1 applies (inline comments
cross-reference `§F685-CORPUS.<n>`; this playbook uses `rg <pattern>` hints for
source references).

---

## §F685-CORPUS.1 — Purpose and cross-cutting nature

The corpus is **shared infrastructure** across multiple `#685` deliverables:

- **F9 promotion gate** — `check_hang` productive-silence path classification
  measured against `expected_hung_classification` ground truth. Promotion
  criteria require FP < 1% on N ≥ 300 not-stuck fixtures (statistical Rule of
  Three at 95% confidence) + 2-week shadow telemetry stable.
- **F39 mitigation selection** — six hypotheses (a)/(b)/(c)/(d)/(e)/(f) in
  `docs/HUNG-STATE-TRANSITIONS.md §F39.4` need FP measurement to pick a winner.
  Same corpus, different harness pass (per §F685-CORPUS.4).
- **Recovery calibration** — the live Stage-1 recovery action remains
  shadow-by-default and needs confidence in detection FP/FN before promotion.

The corpus does **not** belong exclusively to F9 or F39 — it is the
**measurement substrate** both rely on. These explicit contract sections and
the top-level integration test keep that cross-cutting role visible.

## §F685-CORPUS.2 — Manifest schema extension

`ReplayFixture` at `rg "struct ReplayFixture" src/state/tests.rs` has seven
optional fields (serde defaults preserve backward compatibility with schema-v1
fixtures):

| Field | Type | Purpose |
|---|---|---|
| `scenario_kind` | `Option<String>` | One of: `scrollback_static`, `screen_change_same_state`, `priority_oscillation`, `productive_marker_fire`, `productive_silence`, `silent_stuck`, `productive_bursty`. Drives harness measurement dispatch. |
| `expected_hung_classification` | `Option<String>` | Ground truth for F9 promotion measurement. One of: `not_hung`, `hung`, `ambiguous`. |
| `expected_oscillation_count` | `Option<u32>` | F39 measurement: how many priority transitions the trace should produce when wall-clock injection is enabled (deferred — §F685-CORPUS.6). |
| `productive_marker_expectations` | `Vec<{time_ms, source}>` | F9 detailed measurement: which markers fire at which times. Default empty for fixtures without an expectation. |
| `capture_kind` | `Option<String>` | Measurement provenance such as `real`, `synthetic`, or `synthetic_from_real_template`. Drives source-separated reporting per §F685-CORPUS.4. |
| `provenance` | `Option<String>` | Human-readable origin: PR number, operator-session note, or `synthetic from <template>`. Audit trail. |
| `schema_version` | `u32` (default `1`) | Future-compatibility marker. **No runtime enforcement in Phase 1** — informational only; future schema changes bump it and add a migration. |

**Backward compatibility:** schema-v1 fixtures parse unchanged via serde
defaults. The `state::tests::replay_manifest_regression` test pins this path.

## §F685-CORPUS.3 — Initial corpus (historical launch snapshot)

> The counts and backend names in this section describe the Phase 1 launch
> plan. They are retained to explain the original measurement design. Use the
> current-corpus banner above and `MANIFEST.yaml` for live coverage.

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
explicitly as known-stuck backends). Gemini was later retired in favour of Agy.
The current corpus adds Agy coverage, while Grok remains the active backend with
no labelled or schema-v1 fixture.

This initial set is **not** statistically sufficient for the FP < 1% / FN < 10%
gates. Corpus growth is delegated to operators and follow-up sub-tasks per
§F685-CORPUS.6.

## §F685-CORPUS.4 — Measurement methodology

### Two pipelines on the shared corpus

1. **F9 productive-signal pipeline** (active).
   - Replay each schema-v2 fixture through `VTerm` + `infer_productivity`.
   - Compare the resulting `ProductivitySignal` with the `scenario_kind`
     expectation:
     - `productive_marker_fire` → expect `Productive { source: Marker(_) }` or
       `Productive { source: Heartbeat }`
     - `productive_silence` → expect `NoSignal`
     - `silent_stuck` → expect `NoSignal`
   - Smoke test: `rg "corpus_measurement_smoke_f9_marker_signals" src/state/tests.rs`.
2. **F39 oscillation pipeline** (deferred; see §F685-CORPUS.6).
   - Replay each schema-v2 fixture through `StateTracker::feed` with wall-clock
     injection between chunks.
   - Compare the observed transition count with `expected_oscillation_count`.
   - This requires a harness extension that backdates `since` from per-chunk
     timing metadata in the manifest. It was not part of Phase 1.

### Per-transition unit (not per tick)

A single false-Hung transition × 100 ticks counts as **one** FP event. The
classifier returns `true` only on the entry transition (sub-task 1 invariants
5a/5b); harness aggregation mirrors this.

### Source separation in reporting

Reports break out into three lines:

```
F9 measurement (N at report time):
  Real:        X/Y  (high signal value — actual operator sessions)
  Synthetic:   X/Y  (specific scenario coverage — crafted to exercise paths)
  Combined:    X/Y  (aggregate)
```

A synthetic FAIL is immediately actionable (the marker or pattern is wrong).
A real PASS provides ground confidence. A **real FAIL is the most valuable data
point** — it surfaces a production-relevant FP or FN.

The integration test
`rg "corpus_count_report" tests/fixture_corpus_measurement.rs` emits this report
via `eprintln!` (visible with `cargo test -- --nocapture`) and asserts gentle
gates on `scenario_kind` shape (at least one of each core kind). Strictness
ratchets up as N grows.

### Statistical minimums (delegated to corpus growth)

- **FP < 1%** at 95% confidence (Rule of Three): N ≥ 300 not-stuck fixtures
- **FN < 10%** at reasonable confidence: N ≥ 30 known-stuck fixtures

Phase 1 shipped N = 3 schema-v2 fixtures plus the harness; the current manifest
has N = 8 labelled fixtures. **The harness reports rates against current N**;
the promotion criteria (in the F9 commit message and F39 audit) require
`N ≥ minimum AND rate < threshold`. This deliberately reframes the issue's
`FP < 1%` wording from “hit the bar in one PR” to “hit the bar through corpus
growth over time.”

### Shadow versus active F9 measurement

- **Shadow mode** (default, `AGEND_PRODUCTIVE_GATE` unset): F9 telemetry fires,
  but classification is unchanged. This estimates FP rate without production
  impact.
- **Active mode** (`AGEND_PRODUCTIVE_GATE=1`): F9 actually classifies.
  **Required for promotion-criteria measurement.** Test code uses
  `with_f9_gate(true, || { ... })` from `tests/common/env_gate.rs` (and its
  unit-test mirror in `src/health.rs::tests`).

## §F685-CORPUS.5 — Capture workflow

Use the operator-side recipes below; no new CLI tool is required. Synthetic
fixtures may use `printf '%b' ...` to write crafted byte sequences (see git log
on the F685 fixtures for examples).

The generic recording loop is:

```sh
script -q /tmp/<backend>-session.raw <cli-command>
# interact: trigger thinking, let it complete, exit
# copy the file into tests/fixtures/state-replay/ and add a manifest entry
```

For F9/F39 measurement, add the schema-v2 measurement fields in addition to
the ordinary capture metadata:

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

## Setup Checklist

Before starting the capture session, prepare the environment:

- [ ] Create a throwaway working directory:
  ```bash
  export CAPTURE_DIR=$(mktemp -d /tmp/agend-fixture-capture-XXXXX)
  cd "$CAPTURE_DIR"
  git init  # some backends require a git repo
  ```

- [ ] Verify target backend binary is on PATH:
  ```bash
  which claude && claude --version
  which codex && codex --version
  which kiro && kiro --version
  which opencode && opencode --version
  which agy && agy --version
  which grok && grok --version
  ```

- [ ] Back up backend config if you need a clean-state capture:
  ```bash
  # Only if needed for trust-prompt / first-run scenarios
  mv ~/.claude ~/.claude.bak 2>/dev/null
  mv ~/.codex ~/.codex.bak 2>/dev/null
  ```

- [ ] Confirm `script` syntax (macOS BSD vs GNU):
  ```bash
  # macOS BSD (default):
  script -q /tmp/test-capture.raw claude --version
  # GNU (Linux):
  script -q -c "claude --version" /tmp/test-capture.raw
  ```
  All recipes below use **macOS BSD** syntax. On Linux, swap to
  `script -q -c "<command>" <output-file>`.

- [ ] Prepare MANIFEST.yaml entry template (copy this for each capture):
  ```yaml
  - file: <backend>-<scenario>.raw
    backend: <backend-name>
    cli_version: "<version>"
    recorded_on: "<YYYY-MM-DD>"
    scenario: "<one-line description>"
    expected_transitions: [starting, ...]
    expected_final_state: <state>
    expected_final_detect: <state-or-null>
    capture_kind: real_pty
    provenance: "<issue-ref> batch capture by operator"
  ```

---

## Per-Scenario Recipes

### Priority 1: Phase 2a Urgent

#### R1. `claude-yes-proceed.raw` (~3 min)

**Goal**: Capture the "Yes, proceed" confirmation modal that appears when
Claude Code asks for permission (e.g. `--dangerously-skip-permissions`
confirmation or an update prompt). Needed for Phase 2a keystroke audit.

> **#1546 use**: this is the edit/confirm permission class. Its footer + option
> chrome (`Esc to cancel · Tab to amend`, numbered `❯ 1. Yes / … / N. No`) is the
> zero-FP detection anchor #1546 keys on. Capture R1/R2/R2b together so #1546 can
> see whether that chrome is STABLE across permission types (edit vs trust vs
> bash) — `Tab to amend` is edit-specific, so a single footer anchor may not
> cover all of them.

```bash
# 1. Start capture
script -q "$CAPTURE_DIR/claude-yes-proceed.raw" claude

# 2. Trigger: type a request that causes a "Yes, proceed" confirmation.
#    Alternatively, if an update is available, the update-now prompt
#    should show "Yes, proceed" as an option.

# 3. DO NOT dismiss the modal yet -- let it sit for 2-3 seconds so the
#    full ANSI rendering is captured.

# 4. Press Ctrl-C or type /exit to end the session.
```

**Verify**: `xxd claude-yes-proceed.raw | grep -c '1b\['` should show
ANSI escape sequences. The file should contain the literal string
"Yes, proceed".

```bash
grep -c "Yes, proceed" "$CAPTURE_DIR/claude-yes-proceed.raw"
# Expected: >= 1
```

**Time estimate**: ~3 min

---

### Priority 2: #996 Framework Support

#### R2. `claude-trust-prompt.raw` (~2 min)

**Goal**: Capture the trust-folder modal that appears when Claude Code is
launched in an untrusted directory for the first time. Needed for
composite-signature discriminator calibration.

> **#1546 use**: the trust-folder modal may render DIFFERENT footer/option chrome
> than the edit permission (R1) — its wording is "Do you trust the files…", not
> "Tab to amend". #1546 needs this to decide whether one footer anchor covers all
> permission types or each (edit / trust / bash) needs its own chrome match.

```bash
# 1. Ensure clean state (no prior trust for this dir)
rm -rf /tmp/agend-untrusted-test && mkdir /tmp/agend-untrusted-test
cd /tmp/agend-untrusted-test && git init

# 2. Start capture
script -q "$CAPTURE_DIR/claude-trust-prompt.raw" claude

# 3. Wait for the trust modal to appear ("Do you trust the files in
#    this folder?" or "Yes, I trust the files in this folder").
#    Let it render fully (2-3 sec).

# 4. Press Ctrl-C to exit WITHOUT dismissing (we want the modal bytes).
```

**Verify**: file should contain "trust" and ANSI box-drawing characters.

```bash
grep -c "trust" "$CAPTURE_DIR/claude-trust-prompt.raw"
```

**Time estimate**: ~2 min

---

#### R2b. `claude-bash-perm.raw` (#1546 — bash-command permission, ~2 min)

> Numbered `R2b` because it joins the permission group (R1 edit, R2 trust);
> `R3`–`R10` are the productive-marker captures below.

**Goal**: Capture the permission dialog Claude Code shows before running a
**shell command**. #1546 needs to know whether the bash-permission footer/option
chrome matches the edit-permission footer (`Esc to cancel · Tab to amend`) or
differs — `Tab to amend` is edit-specific, so bash may render a different footer.
This decides whether a single footer anchor is enough for #1546's detection or
each permission type needs its own chrome match.

> ⚠ **Do NOT use `--dangerously-skip-permissions`.** Bypass mode skips the
> permission prompt entirely — that is exactly why fleet agents (which run with
> bypass) never see it and cannot self-capture this fixture. Launch Claude
> WITHOUT bypass.

```bash
# 1. Throwaway repo (keep the capture clean)
rm -rf /tmp/agend-bash-perm-test && mkdir /tmp/agend-bash-perm-test
cd /tmp/agend-bash-perm-test && git init

# 2. Start capture — NON-bypass claude
script -q "$CAPTURE_DIR/claude-bash-perm.raw" claude

# 3. Ask it to run a shell command, e.g. type:   run: ls -la
#    The bash-permission dialog renders. Let it render fully (2-3 sec).
#    DO NOT select an option.

# 4. Press Ctrl-C to exit WITHOUT answering (we want the dialog bytes).
```

**Verify**: ANSI escapes present, and dump the footer/option wording so #1546 can
compare chrome across permission types:

```bash
xxd "$CAPTURE_DIR/claude-bash-perm.raw" | grep -c '1b\['
grep -aoE "Esc to cancel[^|]*|Tab to amend|enter to confirm|Allow|Do you want" \
  "$CAPTURE_DIR/claude-bash-perm.raw" | sort -u
```

**MANIFEST entry**:

```yaml
- file: claude-bash-perm.raw
  backend: claude-code
  cli_version: "<version>"
  recorded_on: "<YYYY-MM-DD>"
  scenario: "bash-command permission dialog (run shell command, dialog not answered)"
  expected_transitions: [starting, permission]
  expected_final_state: permission
  expected_final_detect: permission
  capture_kind: real_pty
  provenance: "#1546 operator capture"
```

**Time estimate**: ~2 min

---

### Priority 3: Productive Marker Captures (#1014 / S2)

For each backend, we need two scenarios:

- **productive_marker_fire**: tool result returns and a visible marker
  appears on screen (e.g. file-write confirmation, command output).
- **productive_silence**: long pause in an active session with no visible
  progress markers (the "silent stuck" case for hung detection).

#### R3. Claude productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/claude-productive-marker.raw" claude

# 1. Ask Claude to create a small file:
#    "Create a file called hello.txt with the content 'hello world'"
# 2. Wait for the tool use to complete and the file-write confirmation
#    to appear on screen.
# 3. Let the response finish fully.
# 4. Type /exit
```

#### R4. Claude productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/claude-productive-silence.raw" claude

# 1. Ask a complex question that requires long thinking:
#    "Explain the mathematical proof of Fermat's Last Theorem in detail"
# 2. Wait 30-60 seconds while Claude is thinking/streaming (spinner
#    visible, no tool-use markers).
# 3. Press Ctrl-C mid-response to capture the "stuck" state.
```

#### R5. Codex productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/codex-productive-marker.raw" codex

# 1. Ask Codex to create a file.
# 2. Wait for the apply-patch confirmation and completion.
# 3. Type /exit
```

#### R6. Codex productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/codex-productive-silence.raw" codex

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking/streaming.
# 3. Press Ctrl-C mid-response.
```

#### R7. Agy productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/agy-productive-marker.raw" agy

# 1. Ask Agy to create a file.
# 2. Wait for the tool completion marker.
# 3. Exit the session.
```

#### R8. Agy productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/agy-productive-silence.raw" agy

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during generation.
# 3. Interrupt mid-response.
```

#### R9. Kiro productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/kiro-productive-marker.raw" kiro

# 1. Ask Kiro to create a file.
# 2. Wait for the file-write tool completion marker.
# 3. Type /exit
```

#### R10. Kiro productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/kiro-productive-silence.raw" kiro

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking.
# 3. Press Ctrl-C mid-response.
```

#### R11. OpenCode productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/opencode-productive-marker.raw" opencode

# 1. Ask OpenCode to create a file.
# 2. Wait for the tool completion.
# 3. Type /exit
```

#### R12. OpenCode productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/opencode-productive-silence.raw" opencode

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during thinking.
# 3. Press Ctrl-C mid-response.
```

#### R13. Grok productive_marker_fire (~5 min)

```bash
script -q "$CAPTURE_DIR/grok-productive-marker.raw" grok

# 1. Ask Grok to create a file or run a visible tool action.
# 2. Wait for the completion marker.
# 3. Exit the session.
```

#### R14. Grok productive_silence (~5 min)

```bash
script -q "$CAPTURE_DIR/grok-productive-silence.raw" grok

# 1. Ask a complex question.
# 2. Wait 30-60 seconds during generation.
# 3. Interrupt mid-response.
```

---

## Post-Capture Workflow

### 1. Sanitize

Review each `.raw` file for sensitive content before committing:

```bash
# Check for API keys, tokens, personal paths
for f in "$CAPTURE_DIR"/*.raw; do
  echo "=== $(basename $f) ==="
  strings "$f" | grep -iE 'api.key|token|secret|password|/Users/[^/]+' | head -5
done
```

If sensitive content is found, re-capture in a clean environment or
use `sed` to replace specific strings (preserving byte offsets is
critical -- only replace content that doesn't affect ANSI escape
positioning).

### 2. Copy to fixture directory

```bash
cp "$CAPTURE_DIR"/*.raw tests/fixtures/state-replay/
```

### 3. Add MANIFEST.yaml entries

For each new fixture, add an entry to `tests/fixtures/state-replay/MANIFEST.yaml`
using the template from the setup checklist. Fields to fill:

- `file`: filename (e.g. `claude-yes-proceed.raw`)
- `backend`: backend identifier (`claude-code`, `codex`, `kiro-cli`, `opencode`, `agy`, `grok`)
- `cli_version`: exact version string from `<backend> --version`
- `recorded_on`: today's date in YYYY-MM-DD
- `scenario`: one-line description of what was captured
- `expected_transitions`: leave as `[starting]` initially; fill after replay test
- `capture_kind`: `real_pty`
- `provenance`: reference the batch capture session and issue number

### 4. Verify with replay harness

```bash
cargo test --test state_replay -- --nocapture
```

If a fixture doesn't match expected transitions, update the MANIFEST
entry -- the fixture is ground truth, not the expectation.

### 5. PR shape

- Branch: `fixtures/<batch-description>`
- Files: `.raw` fixtures + MANIFEST.yaml updates only (no src changes)
- Title: `fixtures: batch capture for #<issue> (<backend list>)`
- Cross-ref: link to the issue(s) this batch unblocks

---

## Time Estimates

| Recipe | Time |
|--------|------|
| R1. claude-yes-proceed | ~3 min |
| R2. claude-trust-prompt | ~2 min |
| R3-R14. productive markers (6 backends x 2) | ~60 min |
| Post-capture sanitize + MANIFEST | ~15 min |
| **Total session** | **~80 min** |

Tip: batch the productive captures per-backend to minimize context
switching. Do all claude captures, then all codex, etc.

---

## §F685-CORPUS.6 — Corpus growth protocol and open questions

### Growth protocol

The corpus grows **incident-driven** over weeks:

1. An operator encounters a stuck-in-thinking or false-Hung incident in
   production.
2. Capture (or reproduce) a PTY trace with the §F685-CORPUS.5 workflow.
3. The operator (or a follow-up sub-task) adds a manifest entry with measurement
   labels.
4. The harness re-runs in the next CI cycle; the aggregate FP/FN report updates.
5. When N reaches the statistical minimum **and** rates are below threshold,
   the promotion gate (F9 default-active flip or F39 mitigation choice) unblocks.

Each new fixture is a **follow-up sub-task** of `#685` (not a PR of its own
unless code or harness changes are bundled).

### Open questions

- **Time-injection harness extension:** F39 Scenario C measurement requires
  wall-clock advancement between byte chunks (the priority `min_hold` gates at
  `rg "min_hold" src/state/mod.rs` use `Instant::now()`). The replay loop runs in
  microseconds; even a 30-second real trace replays instantly, so
  `since.elapsed()` never crosses `min_hold`. Needed: per-chunk timestamp
  metadata in a `.raw` companion plus a harness that backdates `since` per
  chunk. This was outside Phase 1; F39 mitigation selection blocks on it.
- **Real Scenario C capture:** not yet obtained. Operators who encounter
  oscillation should script-capture the session and contribute a real fixture.
  A synthetic-from-real-template trace (a timeline-faithful byte sequence based
  on an operator incident report) is acceptable in the interim.
- **Per-backend marker calibration:** deliverable #4 (sub-task 6, decision
  `d-20260514022917793418-0`) shipped backend-specific marker caches, later
  renamed from Gemini to Agy. Grok currently uses the generic cache; see
  `docs/HUNG-STATE-TRANSITIONS.md §F9.2` for current listings. Codex and
  OpenCode markers remain **synthetic-only** pending real PTY captures through
  this growth protocol; their fixtures join the same harness loop once captured.
- **Cargo feature-gate revisit:** Phase 1 ships an always-on harness (zero cost
  when fixtures lack measurement labels). If the corpus grows past about 100
  fixtures and aggregate replay time approaches the CI budget, reconsider
  gating the harness behind `cargo test --features f9-measure`.

## §F685-CORPUS.7 — Cross-references and boundaries

- S2 memo capture protocols: `/tmp/dialectic-996-s2-signatures-dev.md` sections 2.1-2.4
- MANIFEST.yaml recording protocol: header comment in `tests/fixtures/state-replay/MANIFEST.yaml`
- Fixture corpus measurement: `tests/fixture_corpus_measurement.rs`
- Existing real-PTY fixtures: `codex-update.raw` (2026-04-20), `kiro-tooluse.raw` (2026-04-20), `agy-thinking.raw` (2026-05-20)
- `docs/HUNG-STATE-TRANSITIONS.md §F39.5` points here for fixture-corpus capture criteria.
- `docs/HUNG-STATE-TRANSITIONS.md §F9.5` points here for promotion-measurement methodology.
- `src/state/tests.rs::corpus_measurement_smoke_f9_marker_signals` — unit-test smoke harness for F9 marker measurement.
- `tests/common/env_gate.rs::with_f9_gate` — integration-test helper for F9 env-var serialisation. Its mirror in `src/health.rs::tests::with_f9_gate` must stay in lockstep.

### Out of scope

- F39 mitigation (a)/(b)/(c)/(d)/(e)/(f) selection — requires corpus growth and
  the time-injection harness first.
- F9 promotion flip — requires corpus growth and active-mode measurement first.
- Per-backend tuning (deliverable #4) — a separate sub-task.
- Recovery automation beyond the current Stage-1-only dispatcher — requires a
  fresh scope decision and new evidence; removed Stages 2/3 are not a live plan.
- Schema migration code for `schema_version` enforcement — Phase 1 metadata only.
- `cargo test --features f9-measure` gating — defer until N is about 100.
