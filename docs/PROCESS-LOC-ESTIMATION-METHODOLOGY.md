# LOC estimation methodology + ceiling enforcement protocol

**Sprint 60 W3 PR-1 — process doc.** Closes the recurring estimate-miss
pattern observed across Sprint 59-60 (4 incidents, ~17%–5x overrun).
Pairs with the upcoming optional CI helper (`scripts/check_loc_overrun.sh`,
deferred to Sprint 61 per the Component-2-out-of-budget guidance below)
that would automate the soft-warn / hard-fail thresholds described
here.

---

## 1. Why this exists

Lead's per-PR LOC estimates are the budget contract reviewers and
dispatchers anchor on. When estimates miss systematically, three
downstream effects compound:

1. **Reviewer adjudication overhead** — each cohesion-accept call
   takes a separate dispatch + reviewer cycle (15–60 min wall) that
   wouldn't be needed if the estimate had bracketed the actual.
2. **PR description rework** — every overrun-disclosure note is
   another reviewer-grep surface that risks Path-B-style false-positive
   stale-content flags (see `feedback_path_b_doc_clean_removal.md`).
3. **Wave / sprint ETA drift** — when PR estimates underforecast,
   wave ETAs (set by summing PR estimates) underforecast too, and
   the operator-facing sequence calendar slips.

Sprint 59-60 incident table (4 PRs, observed Sprint 60 W3 PR-1
dispatch m-20260509223351835031-404):

| PR                                   | Estimate         | Actual    | Overrun                 |
|--------------------------------------|------------------|-----------|-------------------------|
| #574 (telegram topic cleanup F2)     | 200–330 LOC      | 1008 LOC  | **3–5x**                |
| #576 (helper-staleness watchdog)     | 110–200 LOC      | 331 LOC   | **65% over upper bound**|
| #580 (operator restart MCP)          | 130–230 LOC      | 269 LOC   | **17% over upper bound**|
| #581 (Skills System Plan IMPL)       | 700–1100 LOC     | 944 LOC   | **14% under upper bound** (counterexample)|

3 of 4 over upper bound; magnitudes range 17% → 5x. The
counterexample (#581) used a deliberately wider range (700–1100)
that bracketed the actual — supporting the hypothesis that wider,
honestly-derived ranges are healthier than narrow point-estimates
that get blown.

---

## 2. The 5-category framework

Estimate by decomposing the PR into these categories. Sum the
per-category ranges. Honest decomposition usually yields a *wider*
range than instinct — that's the goal.

### (a) Boilerplate

CLI subcommand wiring, module declarations, dispatch arms, schema
field additions. Cheap per-instance but adds up:

- New `clap::Subcommand` enum + dispatch arm: ~25–40 LOC per
  subcommand (variant + `Some(Commands::X { .. }) => cli::run_x(...)`
  arm + handler stub).
- New `mod foo;` declaration in parent module: 1 LOC, but if it
  triggers an alphabetical-order rebalance check, sometimes 2–3 LOC.
- New MCP tool `tools.rs` schema entry: ~10–25 LOC (description +
  inputSchema + flag).
- New MCP handler dispatch arm in `src/mcp/handlers/mod.rs`: 2–4 LOC.
- `match` arm extension on existing enum: 1–3 LOC per variant.

### (b) Test density

Per-module test density observed in Sprint 56-60:

- Pure-logic module (no IO): tests ~20–30% of prod LOC.
- IO-touching module (filesystem / inbox / lock files): tests
  ~40–60% of prod LOC. Setup helpers + cleanup multiply.
- Cross-platform module (`cfg(unix)` / `cfg(windows)` branches):
  tests ~50–80% of prod LOC. Separate-platform happy paths + at
  least one shared-platform integration test.
- Test-fixture-heavy module (e.g. `setup_git_repo`-style helpers):
  add a flat ~50–100 LOC for the fixture helpers themselves.

### (c) New module vs refactor

- **New module**: budget the full surface (struct + impls + helpers
  + module-doc header + tests). ~150–400 LOC for a module that
  introduces a new responsibility.
- **Refactor / extension**: budget only the diff (existing tests
  may absorb some of the new behavior; pre-existing helpers reduce
  boilerplate). ~30–150 LOC for an extension to a stable module.

Rough rule: new modules typically 2–3× refactor LOC for equivalent
feature surface, because of module doc + struct definitions +
test-helper duplication that an extension PR would skip.

### (d) Cross-platform branches

Each platform fork is a multiplier, not an addition:

- Single `cfg(unix)` + `cfg(not(unix))` symlink-vs-copy fork: +30–60
  LOC for the fork itself + tests on both paths.
- Three-way Windows / macOS / Linux divergence (e.g. service-manager
  install): +80–200 LOC, often with three test variants.
- "fail-open" semantics (Windows fallback + tests for each): add a
  flat ~40–80 LOC test bracket beyond the prod LOC.

### (e) Schema changes vs API expansion

- fleet.yaml schema field add (single optional field): ~10–30 LOC
  including serde defaults + a doc comment.
- New MCP tool surface (handler + tools.rs entry + dispatch arm +
  count-invariant update + tests): ~80–150 LOC. Add ~50 LOC if the
  tool requires daemon-state plumbing (e.g. shutdown-flag bridge).
- New daemon subsystem (supervisor tracker + sidecar storage + emit
  pipeline): ~150–400 LOC. If it parallels an existing tracker
  pattern, lean to the lower end; if it introduces a novel
  invariant, lean higher.
- New CLI subcommand cluster (e.g. `skills add/remove/list/update/
  install`): ~100–200 LOC for the handler functions + main.rs
  dispatch.

---

## 3. Per-category baseline ranges (Sprint 56–60 observed)

Reference table for picking ranges quickly:

| Category                         | Lower bound | Upper bound | Source PR (observed)            |
|----------------------------------|-------------|-------------|---------------------------------|
| New supervisor tracker           | 150 LOC     | 350 LOC     | #567 / #568 / #572 / #576 / #579|
| New MCP handler (single tool)    | 80 LOC      | 200 LOC     | #571 / #574 / #580              |
| New CLI subcommand cluster (4–5) | 100 LOC     | 250 LOC     | #574 (doctor topics) / #581 (skills) |
| New cross-platform module        | 250 LOC     | 800 LOC     | #581 (5-backend symlink/copy)   |
| Single-tool MCP w/ daemon-state  | 150 LOC     | 300 LOC     | #580 (restart_daemon)           |
| Path B doc-only PR               | 100 LOC     | 250 LOC     | #569 / #573 / #575              |
| New process doc + retrospective  | 150 LOC     | 300 LOC     | #569 / this PR                  |

Skills System (#581) at 944 LOC is the high outlier — 5-backend
coverage + Windows symlink-vs-copy fork + skills-lock.json schema
all in one PR. Lead's 700-1100 estimate bracketed it correctly
because each cross-cutting axis was explicitly summed.

---

## 4. Retrospective walkthrough — PR-IMPL #574

Original estimate: ~200–330 LOC (the largest 3–5x overrun).
Decomposition the original estimate used:

- (α-a) bootstrap pre-check: ~70–110 LOC
- (α-c) delete_topic permission surfacing: ~50–80 LOC
- (γ) doctor topics module: ~80–140 LOC

The reality was 1008 LOC. Apply the 5-category framework
retrospectively:

| Component                                   | Re-estimate | Actual |
|--------------------------------------------|-------------|--------|
| 4-class taxonomy + per-class cleanup actions| 120–180 LOC | ~150   |
| 2 render formats (human + json)            | 60–100 LOC  | ~80    |
| DriftResolution enum + interactive prompt + `--yes`/`--prefer` flags | 60–100 LOC | ~80 |
| Tests at appropriate defensive density (16 tests, `setup_test_repo` helpers, IO-touching → 40–60% of prod) | 350–550 LOC | ~530 |
| CLI handler + main.rs subcommand dispatch  | 80–150 LOC  | ~115   |
| Bootstrap + telegram channel changes       | 70–120 LOC  | ~50    |
| **Re-derived total**                       | **740–1200**| 1008   |

The original miss came from three omissions:

1. **Render-format duplication** (human + json) was never priced in.
2. **Interactive prompt UX wiring** (`--yes` / `--prefer-fleet` /
   `--prefer-registry`) was never priced in.
3. **Test density** was modeled but at the lower end (~25%); IO-
   touching modules' actual was ~50%.

The 5-category framework, applied honestly, would have yielded a
740–1200 range that bracketed the actual.

---

## 5. Ceiling enforcement protocol

### 5.1 PR description requirement

Every IMPL PR must include in its description body:

```markdown
<!-- LOC-EST: <lower>-<upper> -->
```

Lead's dispatch sets the range. Dev may revise the range pre-r0
push if investigation reveals a clean miss (surface to lead first,
get either updated range or option (a) cohesion-accept agreement).

### 5.2 Soft-warn threshold (>130% of upper bound)

If actual LOC exceeds 130% of the dispatched upper bound:

- PR description must include a "Scope-overage transparency"
  section (precedent: #574 / #576 / #580 / Skills System #581 r0).
- Reviewer cohesion-accept option (a) is on the table — reviewer
  adjudicates whether the cohesive single PR holds vs split.
- Standard merge proceeds if reviewer accepts.

### 5.3 Hard-fail threshold (>150% of upper bound)

If actual LOC exceeds 150% of the dispatched upper bound:

- Lead/general escalation required before merge.
- Either: (a) reviewer cohesion-accept (override, as today —
  precedent: #574 ran 3–5x and was accepted on this rationale),
  OR (b) split into multiple PRs.
- Estimate methodology miss should be captured in commit-message
  retrospective so future estimates can absorb the lesson.

### 5.4 Cohesion-accept override always available

The reviewer has authority to accept overruns at any threshold via
"option (a) cohesion-accept" as documented in the Sprint 59 W2
PR-IMPL precedent and reinforced in #576 / #580. The override is
not a free pass — it requires the reviewer to articulate why the
single-PR cohesion is load-bearing for correctness or review
clarity (e.g. cross-platform fork in one place, integration tests
that span the surface).

---

## 6. Out of scope (Sprint 61 candidates)

This PR ships the methodology doc + protocol contract. The
following carry to Sprint 61:

- **`scripts/check_loc_overrun.sh` CI helper** — automates the
  soft-warn / hard-fail thresholds by parsing the
  `<!-- LOC-EST: X-Y -->` marker against `gh pr diff` actual LOC.
  Estimated 60–100 LOC (Component 2 of the original W3 PR-1
  dispatch m-20260509223351835031-404). Deferred per the dispatch's
  own "Component 2 desired but exceeds budget → defer to Sprint 61"
  guidance — applying the framework to this PR's own scope decision
  yielded the doc-only ship.
- **GitHub Actions workflow integration** — wires the helper script
  into the PR check pipeline. Pure-wiring follow-up once Component 2
  ships.
- **Automated reviewer cohesion-accept label** (`loc-overrun-accepted`)
  — reviewer applies via PR label; helper script honors as override.
  Pure-wiring follow-up.

---

**Summary.** 4 Sprint 59-60 estimate-miss incidents motivated this
methodology. 5-category framework (boilerplate / test density / new
module / cross-platform / schema vs API) lets lead decompose
estimates honestly and brackets the actual more often. Ceiling
enforcement protocol (soft-warn 130% / hard-fail 150% / reviewer
cohesion-accept override) preserves the existing reviewer authority
while making overruns visible upfront. Component 2 helper script +
GHA wiring deferred to Sprint 61 per pure-wiring follow-up pattern.
