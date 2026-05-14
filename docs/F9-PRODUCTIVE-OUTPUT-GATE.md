# F9 Productive-Output Gate

Source-of-truth for the F9 sub-finding (`#685` Phase 1 deliverables #2 + #3):
the dual-path supplement to silence-based Hung detection. Companion to the
F9 inline structured comments in `src/state.rs`, `src/behavioral.rs`, and
`src/health.rs`.

Issue: [#685](https://github.com/suzuke/agend-terminal/issues/685) F9 sub-finding.
Sibling sub-tasks: 1 (Hung audit, PR #750), 2 (F39 audit, PR #752), 3 (F39
speculative narrow, PR #763).

Decision chain:
- `d-20260513154400110972-2` (sub-task 1 base — Hung invariants)
- `d-20260513161542381785-0` (sub-task 2 audit — F39 hypotheses)
- `d-20260513231713506833-1` (sub-task 3 speculative — F39 Gemini narrow)
- `d-20260513235514013631-0` (this sub-task — F9 productive-output gate)

Maintenance: section IDs (`§F9.1`-`§F9.5`) are contract anchors. Renaming
must propagate in the same PR. M1/M2/M3 discipline from sub-task 1 applies
(inline comments use `§F9.<n>` refs; this doc uses `rg <pattern>` grep
hints; section headings are stable).

## §F9.1 — Architecture rationale (dual-path supplement)

The naïve framing "replace silence_exceeds_threshold with productive-output
detection" is **wrong**. It would regress `#659` silent-stuck-in-thinking
detection — a process that truly stops emitting any PTY bytes would never
trigger Hung if classification waited for *productive* bytes specifically
(none come; agent stays in pre-Hung state forever).

F9 ships as **dual-path supplement**:

- **Existing silent path** (any output = alive, threshold-based) preserved
  in `check_hang`. Catches the `#659` case where the agent goes truly silent.
- **NEW productive path** supplements: when silence is below threshold
  (the agent is producing *some* output) but no *productive* output has
  arrived past a separate threshold, the agent is flagged as a Hung
  candidate. Catches the F9 grey failure where 1-byte spinner output
  resets the upstream silence timer indefinitely while no real work
  happens.
- `check_hang` returns `true` if EITHER path triggers; the union strictly
  ADDS coverage and never removes it.

This is the layer at which F9 operates: **HealthState** classification.
The sibling F39(c) hypothesis (sub-task 2 §F39.4) operates at the
**AgentState** layer (`Thinking` pattern stickiness) — different concern,
different bug surface. Cross-reference: `docs/HUNG-STATE-TRANSITIONS.md §F39.4`
footnote.

## §F9.2 — Productive-signal design

A signal is **productive** if either:

1. **MCP heartbeat** refreshed recently (`use_heartbeat: true` configs).
   Heartbeat refresh = the agent called an MCP tool, which is concrete
   evidence of forward progress. Universal across managed backends.
2. **Structural marker** matched the rendered screen text. Markers use
   **line-start anchors and specific formats** — NOT bare keywords. The
   bare-keyword approach (`Saved` / `Wrote`) suffers from the same
   scrollback FP surface as the F39 audit's Scenario A/B/C taxonomy.

Generic structural anchors shared by all backends (file save banners):
- `^Saved to \S+` — file save banner
- `^Wrote \d+ bytes` — explicit byte count
- `^Created file: \S+` — structured creation

Per-backend completion markers (shipped by `#685` sub-task 6 —
deliverable #4, decision `d-20260514022917793418-0`). Each backend's
`MARKERS` const lists the generic anchors above plus its own
completion-glyph + tool-vocab regex. Look up each via
`rg "<BACKEND>_PRODUCTIVE_MARKERS" src/behavioral.rs`:

| Backend | Completion regex (added to generic anchors) | Source | Validation |
|---|---|---|---|
| Claude | `^[✓●⏺]\s+(Read\|Bash\|Edit\|Write\|Grep\|Glob\|Listing\|Reading\|Writing\|Searching\|Editing)\b` | `rg "Read\|Bash\|Edit\|Write\|Grep" src/state.rs` (state.rs:212 ToolUse vocab) | F685 fixture `f685-f9-positive-savedfile.raw` (synthetic). Real captures pending corpus growth. |
| Kiro | `^●\s+(Read\|Write\|Edit\|Bash\|Grep\|Glob\|Task\|List\|Search)\b` PLUS `\[(fs_read\|fs_write\|execute_bash)\]` | `rg "●" src/state.rs` (state.rs:261 ToolUse vocab + bracket-form fs/exec tools) | Synthetic unit tests only — `kiro_markers_*` in `src/behavioral.rs::tests`. **Not validated against real captures** — corpus growth path per F685. |
| Codex | `^•\s+(Explored\|Edited\|Ran)\b` PLUS `apply_patch` | `rg "Explored\|Edited\|Ran" src/state.rs` (state.rs:308 past-tense title vocab + `apply_patch` literal) | **Synthetic only — not validated against real captures.** Corpus growth path. |
| Gemini | `^✓\s+(ReadFile\|WriteFile\|ReadManyFiles\|Edit\|Shell\|WebFetch\|Glob\|GoogleSearch\|MemoryTool\|ReadFolder)\b` | `rg "ReadFile\|WriteFile" src/state.rs` (state.rs:405 CamelCase ToolUse vocab) | F685 fixture `f685-silent-stuck-stub.raw` (synthetic_from_real_template). Real captures pending. |
| OpenCode | `^→\s+(Read\|Write\|Edit\|Glob\|Grep\|Bash\|List\|Task)\b` | `rg "→" src/state.rs` (state.rs:357 completion arrow — distinct from in-flight `✱`) | **Synthetic only — not validated against real captures.** Corpus growth path. |

**Excluded from F9 markers**:
- All in-progress / spinner glyphs (Braille `[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏]`, OpenCode `✱`, Codex `◦ Working`,
  Gemini `⠦ Thinking`). F9 productivity = completion-only; these fire BEFORE work completes.
- Gemini `tool.*call` / `MCP.*tool` literals — heartbeat path already covers MCP signals
  (avoid double counting).
- Bare keyword markers (e.g. `Saved` / `Wrote` without line-start anchor). Prose like
  "I saved your time" must not match — pinned by
  `infer_productivity_rejects_bare_keyword_scrollback` (legacy) and
  `<backend>_markers_reject_*` per-backend tests.

### Cache routing

Per-backend markers are NOT routed via pointer-eq (would fall through
to per-call `Regex::new()` compile — the bug that caused PR #766's
ubuntu/windows CI failure). Sub-task 6 introduced the `MarkerCacheId`
enum on `ProductivityConfig`:

```rust
pub enum MarkerCacheId { Generic, Claude, Kiro, Codex, Gemini, OpenCode }

pub struct ProductivityConfig {
    pub markers: &'static [&'static str],
    pub use_heartbeat: bool,
    pub heartbeat_fresh_window_ms: u64,
    pub cache_id: Option<MarkerCacheId>,
}
```

`infer_productivity` matches on `cache_id` to route to the
corresponding per-backend `LazyLock<Vec<Regex>>` static
(`CLAUDE_PRODUCTIVE_REGEXES`, etc.). Compile-time exhaustive match
prevents missing-backend bugs. `None` is reserved for ad-hoc test
configs and falls back to `Regex::new()` per call (Phase 1 production
code never hits this path).

Future per-backend `heartbeat_fresh_window_ms` tuning and per-backend
silence threshold tuning are **out of scope for sub-task 6** — both
require corpus measurement data (see §F9.5 promotion criteria) before
calibration.

## §F9.3 — Dual-path decision table

| `silent` | `silent_productive` | `agent_state` | Default mode | `AGEND_PRODUCTIVE_GATE=1` |
|---|---|---|---|---|
| ≤ threshold | ≤ threshold | any | not-Hung | not-Hung |
| > threshold | any | non-Idle | discriminator (existing) | discriminator (existing) |
| ≤ threshold | > threshold | non-Idle | not-Hung + telemetry | discriminator (NEW path) |
| any | any | `Idle` | not-Hung (Idle never hangs) | not-Hung (Idle never hangs) |

"discriminator" = the existing input-pending-past-heartbeat /
heartbeat-fresh / IdleLong branch in `check_hang`. Both paths route into
the same discriminator once a path triggers — F9 does NOT add a new
discriminator branch, only a new entry condition.

Find threshold mapping in source: `rg "silence_exceeds_threshold" src/health.rs`
(silent path) and `rg "productive_silence_exceeds" src/health.rs` (F9 path);
both use the same per-`AgentState` thresholds in this PR (Phase 1 minimal —
deliverable #4 may diverge them).

## §F9.4 — Known limitations (to be measured by fixture corpus)

### 4.1 Heartbeat-as-productive gap
Pure-reasoning long sessions (e.g. Claude internal thinking without MCP
tool calls) generate no heartbeat refresh **and** no productive markers.
Once `AGEND_PRODUCTIVE_GATE=1` is set, these sessions are flagged Hung
after threshold despite legitimate work in progress.

**Mitigation deferred**:
- Follow-up F9 sub-task integrating spinner-cycling-as-productive (the
  F39 hypothesis (e) variant — pattern-source-line tracking, but inverted
  to count spinner-glyph activity as evidence).
- Operator override mechanism (out of F9 scope).

**Risk-contained**: shadow-mode + env-var opt-in means only opted-in
users encounter this FN during F9 rollout. Activation is gated on
fixture corpus measurement (§F9.5).

### 4.2 Generic markers FP residual
Even with structural anchors, edge cases remain. A user pasting the
literal string `Saved to /tmp/foo` into a chat message (where the agent
echoes input back) could match the marker. Pinned by the negative test
`infer_productivity_rejects_bare_keyword_scrollback` — anchor approach
substantially narrows the surface, but does not eliminate it.

**Mitigation applied in this PR**: line-start anchors (`^`) +
specific formats. Tests pin both the positive (real markers match) and
negative (bare prose does not) contracts.

**Residual risk acknowledged**: documented for fixture-corpus measurement.

### 4.3 Cross-backend pattern uniformity
Phase 1 ships the same generic markers across all backends. Per-backend
calibration (kiro `[fs_read]`, Gemini-specific tool banners, etc.) lives
in deliverable #4. Backends whose progress markers differ from the
generic set will have lower F9 sensitivity in Phase 1.

## §F9.5 — Activation gate (shadow → opt-in → promotion)

F9 productive-silence telemetry fires unconditionally (`rg "F9 dual-path candidate" src/health.rs`).
**Classification** is gated on the env var `AGEND_PRODUCTIVE_GATE`:

```
unset / not "1"   → shadow mode (default): telemetry collected,
                    no Hung classification from productive path
"1"               → active mode: productive-silence path can flag Hung
```

**Anti-dead-infra clause**: the Sprint 27 PR-A behavioral telemetry
shipped in shadow mode and never promoted. This PR's commit message
encodes explicit promotion criteria to avoid the same outcome:

1. **Fixture corpus measures FP rate < 1% on 3+ confirmed cases**
   (`#685` issue acceptance criterion). Measurement methodology and
   corpus growth protocol live in
   `docs/F685-FIXTURE-CORPUS.md §F685-CORPUS.4` (per-transition unit,
   source-separated reporting, statistical minimums delegate-to-growth).
2. **2-week shadow-mode telemetry shows behavioral divergence stable**
   (Sprint 27 PR-A divergence dashboard pattern reused via
   `rg "behavioral_shadow" src/behavioral.rs`).
3. **Operator decision to flip the env var default** (separate PR
   that changes the default in `check_hang` from unset → `"1"`,
   or removes the gate entirely once promoted).

Without all three, the gate stays default-off. If the timeline drags
past 6 weeks without measurement, the F9 path itself becomes a candidate
for removal — dead shadow infra is worse than no infra.

### 5.1 Cross-references

- `docs/HUNG-STATE-TRANSITIONS.md §F39.4` hypothesis (c) row footnote —
  layer distinction (F39(c) AgentState vs F9 HealthState).
- `docs/HUNG-STATE-TRANSITIONS.md §Invariants 5b/5c` — F9 preserves the
  `check_hang -> bool` return-type contract and the `display_name()`
  string contract (5c wire-compat). The new `silent_productive: Duration`
  parameter is an internal-API refactor; the sole production caller is
  `src/daemon/per_tick/hang_detection.rs` (`rg "check_hang(" src/daemon`).
- `src/behavioral.rs` — `ProductivitySignal`, `ProductivitySource`,
  `ProductivityConfig`, `config_for_productivity`, `infer_productivity`,
  `log_productivity_telemetry`. Parallel to `BehavioralSignal` /
  `BehavioralConfig` / `infer_from_silence` / `log_shadow_telemetry`
  (Sprint 27 PR-A — silence-side equivalents).
- `docs/RECOVERY-STAGES.md` — `#685` Phase 2 staged auto-recovery
  dispatcher reads `productive_silence_exceeds` helper (extracted
  from `check_hang`) directly to decide Stage 1 alive-stuck vs
  dead-likely branch. Recovery treats all `Hung` sources the same
  regardless of F9 promotion state — see §RS.4.
