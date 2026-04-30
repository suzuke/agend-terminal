# PLAN: Sprint 33 — UI tidy + observation tool

**Date**: 2026-04-29
**Basis**: main HEAD `84a6e21` (post PR #315 merge)
**Operator brief**: `general` m-? at 2026-04-29T10:08Z — three frozen-scope PRs, PLAN-first, Tier-B.
**Team**: lead2 (orchestrator, minimal + cost-benefit) · dev2 / kiro-cli (structural) · reviewer2 / codex (prior-art)
**Status**: ready for review — dev2 (structural) + reviewer2 (prior-art) reports synthesised; §5 surfaces 8 decision points for operator.

---

## 0. Scope (frozen by operator — do NOT relitigate)

Three PRs, in dependency order this plan recommends:

| # | PR | Type | Operator estimate |
|---|---|---|---|
| PR-1 | Remove TUI tab `[state]` suffix | UI deletion | ~50 LOC |
| PR-2 | Remove `tool_kill` MCP tool + tool_kill-only SIGINT pgid path | feature deletion | ~150 LOC |
| PR-3 | New `pane_snapshot(target, lines?)` MCP tool | new feature | ~300 LOC |

Operator decisions baked in (do not challenge):
- PR-1: state display has no real use; tab click region shifts; mis-classification misleads.
- PR-2: `tool_kill` is misuse-prone; "tool_kill 後 dev 卡 restarting 反而害事". Future ESC-unable scenario considered acceptable.
- PR-3: `describe_instance` metadata-only is insufficient; operator hit "I'm blind-推-ing dev's state" failure mode personally.

The 4-perspective challenge round answers **how minimal · prior art · cost-benefit boundaries**, not whether-to.

## 1. PR-1 — Remove TUI tab `[state]` suffix

### 1.1 Minimal-delta path (lead2)

Single-site change at `src/render.rs:613`:

```rust
// before
let base = if pane.backend.is_some() {
    format!(" {} [{}]", pane.label(), state.display_name())
} else {
    format!(" {}", pane.label())
};
```

Becomes:

```rust
let base = format!(" {}", pane.label());
```

Then `state: AgentState` parameter to `pane_title_segments` becomes unused. Either drop it (cleaner), keep with `_` prefix (smallest delta), or — preferred — drop and update both call sites at `:135` and `:492`.

**Effective LOC**: ~10–15 (one branch removed, one parameter dropped, two call sites updated). Operator's ~50 LOC estimate covers ripple cleanup if `pane_title_segments`'s caller chain has more state-coupled formatting elsewhere.

**KISS check**: removal breaks nothing the operator uses. The state info is still available via `describe_instance` and the `:1820` Span listing in the meta panel — not removed by this PR.

### 1.2 Structural impact (dev2)

**Side-effect surface**: single touch point at `src/render.rs:613`. Caller chain:
- `pane_title_segments` only call site is `src/render.rs:539` (inside `render_pane`).
- `state.display_name()` is invoked 10+ times across `instance_monitor`, `daemon`, `api/handlers/query`, `cli`, `supervisor` — all unaffected (only the `:613` site is removed).

**Test impact**:
- `src/render.rs:2135` `pane_title_segments` test asserts `joined.contains("[3]")` for the notification-count badge, NOT the state suffix. Test stays, but its signature input (currently passes `AgentState::Idle`) updates to match the new function signature if we drop the param.
- `src/render.rs:2347` `transient_state_badge` test — tests `[respawning]`/`[crashed]` badges in the **tab bar** (a different surface), not pane title. Unaffected.
- No tests anywhere assert `[idle]` / `[ready]` / `[thinking]` etc. in pane titles.

**Boundary cleanliness**: cleanest cut **drops `state: AgentState` from `pane_title_segments`'s signature**.
- The caller at `render.rs:496` retains `state` for `state_color(state)` (border / title colour) — `pane_title_segments` only receives `title_style: Style` already coloured upstream.
- After the suffix removal, `state` inside the fn body is unused — drop the parameter for clarity, update `:539` call site and `:2135` test input.
- New signature: `fn pane_title_segments(pane: &Pane, title_style: Style) -> Vec<(String, Style)>`.
- No dangling abstractions: `display_name()`, `state_color()`, `transient_state_badge()` all stay (each has independent callers).

### 1.3 Prior-art (reviewer2)

- `pane_title_segments` was introduced in a single commit (`0ce2246`) and has not been structurally changed since. `git log -S "pane_title_segments" -- src/render.rs` returns only `0ce2246`.
- Heavy `render.rs` churn (`e68fbde` tab-bar hit-test abstraction, `a2acf75` robustness, `e968765` stable order) never touched the suffix contract.
- **No prior precedent for removing this exact `[state]` suffix** — net-new at this call site.
- Closest pattern template: render-local mechanical simplifications with narrow blast radius — `e968765`, `a2acf75`. Pair with click-region test(s); avoid state-machine changes.

### 1.4 Cost-benefit boundaries (lead2)

| Question | Recommendation | Rationale |
|---|---|---|
| Also remove `m.agent_state` Span at `src/render.rs:1820`? | **No** — separate scope | Operator scoped to "tab"; meta panel is a different surface and may genuinely use the state column. |
| Also remove `[notification_count]` suffix at `:618-625`? | **No** | Different concept (urgent UI signal); operator did not include in scope. |
| Drop `state` param entirely from `pane_title_segments`? | **Yes** | Dead parameter is worse than no parameter — clarifies the function's contract for future readers. |
| Update call sites or leave them passing the unused arg? | **Update both** (`:135`, `:492`) | Same KISS reasoning. |

## 2. PR-2 — Remove `tool_kill` MCP tool + tool_kill-only SIGINT pgid path

### 2.1 Minimal-delta path (lead2)

Removal sites (file:line confirmed):

| File | Removal |
|---|---|
| `src/mcp/tools.rs:102` | tool registry entry |
| `src/mcp/tools.rs:370` | invariant count `26 → 25` (then `→ 26` if PR-3 follows) |
| `src/mcp/handlers/mod.rs:39-51` | `build_tool_kill_result` + `build_tool_kill_audit` pure helpers |
| `src/mcp/handlers/mod.rs:122` | dispatch arm `"tool_kill" => …` |
| `src/mcp/handlers/instance.rs:316-352` | `handle_tool_kill` fn (37 lines) |
| `src/mcp/handlers/tests.rs:1497-1518+` | 3 helper-fn tests |
| `src/api/mod.rs:142` | `pub const TOOL_KILL: &str = "tool_kill";` |
| `src/api/mod.rs:322` | `method::TOOL_KILL => …` dispatch arm |
| `src/api/handlers/instance.rs:381` (and surrounding fn) | the SIGINT pgid call site |
| `src/behavioral.rs:57,66,78,84,90,96,102` | `supports_fg_pgid` field — see §2.4 (a) |

**Keep** (NOT tool_kill-specific):
- `src/process.rs` killpg/SIGTERM (graceful shutdown)
- `src/connect.rs:140` SIGINT handler (ratatui restore)
- `src/bootstrap/signals.rs` SIGINT/SIGTERM/SIGHUP (terminal restoration)
- `src/api/mod.rs:348` `libc::kill(pid, 0)` (peer-PID liveness check)
- `src/tui.rs:203` Ctrl+C semantic note
- `src/backend_harness.rs verify_tcgetpgrp` — see §2.4 (b)

§3.5.11 #6 pure-deletion exemption applies — single-commit removal with grep-verifiable 0-hit confirmation post-merge. Reviewer attestation: `pure deletion verified: grep -rn "tool_kill\|TOOL_KILL\|build_tool_kill" src/ → 0 hits in production code path`.

### 2.2 Structural impact (dev2)

**7-file removal surface** (table from dev2):

| File | Lines | Item |
|---|---|---|
| `src/mcp/tools.rs:102` | ~5 | tool definition entry in `tool_definitions()` |
| `src/mcp/tools.rs:370+` | 1 | invariant assertion `26 → 25` |
| `src/mcp/handlers/mod.rs:39-51` | 13 | `build_tool_kill_result` + `build_tool_kill_audit` (`#[cfg(unix)]`) |
| `src/mcp/handlers/mod.rs:122` | 1 | dispatch arm `"tool_kill" => …` |
| `src/mcp/handlers/instance.rs:316-352` | 37 | `handle_tool_kill` MCP handler |
| `src/api/mod.rs:142` | 1 | `TOOL_KILL` const |
| `src/api/mod.rs:322` | 1 | API dispatch arm |
| `src/api/handlers/instance.rs:353-387` | 35 | API handler with `libc::kill(-pgid, SIGINT)` call |

**Comment-only updates** (text only, no code change):
- `src/health.rs:52, :210` — escalation-chain doc comments mention `tool_kill`. Update to remove mention.

**`supports_fg_pgid` confirmed write-only / dead** (dev2 + reviewer2 agree):
- 7 writes (1 field def + 6 backend initialisers in `behavioral.rs:57,66,78,84,90,96,102`).
- 0 reads anywhere in `src/` or `tests/`. Safe to remove with PR-2.

**`verify_tcgetpgrp` in `backend_harness.rs` is NOT tool_kill-specific** (dev2 correction to my §2.4 (b) — see revised recommendation there). It's used by behavioural tests for PTY fd access; **keep it**.

**Test impact** (4 tests to remove, not 3 — dev2 found one I missed):

| File:Line | Test | Action |
|---|---|---|
| `src/mcp/handlers/tests.rs:1501` | `test_tool_kill_result_includes_pgid_and_target` | remove |
| `src/mcp/handlers/tests.rs:1511` | `test_tool_kill_result_includes_reason_when_provided` | remove |
| `src/mcp/handlers/tests.rs:1518` | `test_tool_kill_audit_format` | remove |
| `src/mcp/handlers/tests.rs:1525` | `test_tool_kill_target_not_found_returns_error` | remove |
| `src/mcp/tools.rs:370` | `tool_definitions_count_invariant_post_sprint_30` | update `26 → 25` |
| `tests/mcp_roundtrip.rs:144` | `tools.len() >= 25` | passes at 25 (uses `>=`); no change needed |

No `tool_kill` references in `tests/mcp_characterization.rs` or `tests/mcp_proxy_*.rs`.

**Boundary cleanliness**: clean cut. SIGINT-via-pgid path at `api/handlers/instance.rs:381` is **exclusively** tool_kill. No shared callers. No orphaned abstractions after removal.

### 2.3 Prior-art (reviewer2)

**`tool_kill` introduction**: commit `780801f` (Sprint 11). Threat / usage model in the intro commit is **operational interruption** (`SIGINT` foreground pgid), NOT a localhost auth / security boundary defense. **Important distinction**: this aligns with user ergonomics, not the RBAC / paranoia class that `audit-over-engineering-2026-04-28.md` removed in Sprint 29. The §3.5.12 (d) counter-example construction must therefore consider operational/ergonomic counter-examples (does removal block recovery?) rather than threat-model counter-examples.

**Comparable removals — canonical patterns**:

- **PR #285 RBAC deletion** (`266ac9a`) — §3.5.11 #6 pure-deletion exemption canonical, includes grep attestation in commit message. Files removed include `src/channel/auth.rs` + tests.
- **PR #291 Sprint 30 low-value tool removal** (`f65a87c`) — pure deletion of schemas + dispatch arms + dead wrappers + characterization test entries. Explicitly records §3.5.11 #6 attestation + surviving expected references list.
- **Sprint 30 consolidations** (NOT deletions, but adjacent pattern — behavior-preserving): `f448e1d` unified `send` (5→1), `5bad7f3` `inbox` subsumes describe_*, `3b7ccc2` 7 CRUD groups consolidated. All with RED/GREEN routing tests + alias retention windows.

**`supports_fg_pgid` archaeology — confirmed write-only / dead**:
- Field added in commit `9e9ce70` at `src/behavioral.rs:57`, initialised at `:66, :78, :84, :90, :96, :102`.
- `rg "supports_fg_pgid" src tests docs` hits only those definition / initialization lines.
- `git log -p -S"supports_fg_pgid" -- src/behavioral.rs` shows add-only introduction, no later read-path. **Dead since add**.
- Conclusion: §2.4 (a) recommendation upheld — remove with PR-2.

**Tool-count invariant authority**: `src/mcp/tools.rs:367-384`, introduced in `4c326a5`. Currently asserts `count == 26`. PR-2 must update assertion.

### 2.4 Cost-benefit boundaries (lead2 — revised after dev2 + reviewer2 reports)

| Question | Recommendation | Rationale |
|---|---|---|
| (a) Remove `supports_fg_pgid` field from `behavioral.rs`? | **Yes — include in PR-2** | dev2 + reviewer2 both confirm 7 writes / 0 reads. Field was added FOR `tool_kill`; removal is scope-coherent KISS, not creep. |
| (b) Remove `verify_tcgetpgrp` + its test in `backend_harness.rs`? | **No — REVISED, keep** | dev2 correction: `verify_tcgetpgrp` is used by **behavioural tests for PTY fd access**, NOT specifically by tool_kill. Removing it would break behavioural tests. Keep. |
| (c) Update doc-comment mentions of `tool_kill` in `src/health.rs:52, :210`? | **Yes — include in PR-2** | Escalation-chain comments will go stale post-removal. dev2 surfaced the exact lines. |
| (d) Update `docs/architecture.md` / `docs/decisions/`? | **Yes if mentions exist** | Sweep grep in PR-2 commit. |
| (e) Update MEMORY.md cross-instance / agent memory files? | **Yes if mentions exist** | Per operator brief "更新 docs / decisions / MEMORY 提及處". |
| (f) Backwards-compat shim for `tool_kill` (return error JSON)? | **No** | Per §3.5.11 #6 pure-deletion exemption + KISS. Callers see method-not-found, which is the correct signal. |
| (g) Update `docs/ARCHITECTURE-QUICK-START.md` "26 MCP tools" mention? | **Yes — coordinate with PR-3** | Net invariant at sprint-end is still 26 (PR-2 `-1`, PR-3 `+1`); but the doc's reference to non-existent `tests/mcp_tools_count.rs` (per reviewer2 finding, added in `ac5437f`) should be corrected to `src/mcp/tools.rs:367-384`. Land in whichever PR touches the count first. |

### 2.5 §3.5.12 (d) counter-example construction (mandatory for removal PRs)

Protocol anchor: `docs/FLEET-DEV-PROTOCOL-v1.md:499-509`. Operator pre-decided removal, but the protocol requires this section even on operator-mandated removals. **Important framing per reviewer2's prior-art**: tool_kill's intro threat model is operational interruption (not auth / security), so counter-examples must be operational / ergonomic, not paranoia-style. Combined attempts (mine + reviewer2):

1. **Foreground tool process ignores ESC but handles SIGINT** — would favour keep. **Weakness**: existing `interrupt` MCP tool (ESC) + `replace_instance` already provide fallback; this is recovery convenience, not unique safety boundary.
2. **Health escalation chain needs intermediate non-destructive cancel** (interrupt too weak, replace too strong). **Weakness**: no enforced automatic chain requires `tool_kill`; removal does not block eventual recovery path.
3. **Runaway subprocess holds PTY, agent session still valuable** — `tool_kill` could preserve session while killing child. **Weakness**: architecture already tolerates session replacement and handover via `replace_instance` (`src/mcp/handlers/instance.rs:223+`).
4. **Unix operational parity with documented API method** — could argue removal regresses Unix operators. **Weakness**: this is feature-surface regression, not a threat-prevention counter-example under §3.5.12 (d) (which is about defensive mechanism removal).
5. **Speculative future backend benefit** (non-PTY backend with cooperative cancel) — re-introduce when concrete need arises. Speculative future need is exactly what §0 KISS forbids.

**Verdict: 0 of 5 compelling counter-examples** → §3.5.12 (d) gate satisfied → removal authorised. All scenarios degrade to ergonomics gradations, not threat-model counter-examples; matches operator's prior intuition that "tool_kill 後 dev 卡 restarting 反而害事".

### 2.6 Tool-count invariant interaction with PR-3

`src/mcp/tools.rs:370` asserts `tools.len() == 26`. Within PR-2's commit, the assertion must update to `25` to keep CI green. If PR-3 lands second, its commit updates `25 → 26`. **Net invariant value at end of Sprint 33: 26 (unchanged).** Per-PR invariant updates are part of each PR's diff.

## 3. PR-3 — `pane_snapshot(target, lines?)` MCP tool

### 3.1 Minimal-delta path (lead2)

New surfaces:

| File | Addition |
|---|---|
| `src/mcp/tools.rs` | new `pane_snapshot` registry entry; invariant count `25 → 26` (assuming PR-2 merged first) |
| `src/mcp/handlers/instance.rs` (or new `pane.rs`) | `handle_pane_snapshot(home, args) -> Value` |
| `src/mcp/handlers/mod.rs` | dispatch arm `"pane_snapshot" => …` |
| `src/api/mod.rs` | `pub const PANE_SNAPSHOT: &str` const + dispatch arm |
| `src/api/handlers/instance.rs` (or new) | API handler that resolves `target` instance + reads vterm scrollback |
| `src/vterm.rs` | new pub method e.g. `snapshot_text(lines: usize) -> String` walking grid cells |
| Tests | inline `#[test]` per §3.5.11 test-first; integration test in `tests/mcp_*.rs` per §3.5.10 wire-format invariant scope |

Recommended cuts that minimise surface (revised after dev2 + reviewer2 reports):

- **Match the established pattern**: extend `tail_lines`'s shape (cell walk + wide-char handling + trim — `src/vterm.rs:333-380`) to scrollback by writing a sibling `read_scrollback(max_lines)` that walks negative line indices. **`tail_lines` itself does NOT serve PR-3** — it's visible-screen-only — but its shape is the right blueprint (see §3.3 / §3.5).
- Expose at the `VTerm` boundary; keep alacritty internal.
- Default `lines` cap = 100 (~one screen of context), max = 10000 (matches `scrolling_history` config). Reject larger with operator-actionable error.
- **Revised LOC estimate** (with dev2 correction): ~150–220 LOC (new `read_scrollback` ~30–50 + MCP tool registry / handler / API method / tests / invariant update). Lower than operator's ~300 estimate but not as small as the optimistic "just expose tail_lines" reading. Operator to confirm per §5.

### 3.2 Structural impact (dev2)

**7 touch points** (purely additive — no existing tests broken):

| Location | What |
|---|---|
| `src/mcp/tools.rs` | new `pane_snapshot` registry entry |
| `src/mcp/tools.rs:370` | invariant count update (see §2.6) |
| `src/mcp/handlers/mod.rs` | dispatch arm `"pane_snapshot" => …` |
| `src/mcp/handlers/instance.rs` (best fit — instance-scoped) | `handle_pane_snapshot` MCP handler |
| `src/api/mod.rs` | `PANE_SNAPSHOT` const + dispatch arm |
| `src/api/handlers/instance.rs` | API handler that locks registry, reads VTerm |
| `src/vterm.rs` | new `read_scrollback` method (see §3.3 / §3.5) |

**API call chain pattern**: MCP handler → `api::call()` → API handler → `lock_registry` → `handle.core.lock()` → `core.vterm`. This is the established convention (see `handle_list` in `query.rs`, `handle_tool_kill` in `instance.rs` pre-removal). No new architectural patterns introduced.

**Where to put the API handler**: dev2 recommends `api/handlers/instance.rs` (already houses lifecycle + interrupt + move_pane). Acceptable alternative: `api/handlers/query.rs` (read-only family). Recommend `instance.rs` for cohesion with `pane_snapshot`'s instance-scoped target argument.

### 3.3 Prior-art (reviewer2 — clarified vs. dev2)

**🎯 Resolved conflict between perspectives**:

reviewer2 surfaced `tail_lines` (`src/vterm.rs:333-380`, commit `61044d9`) as the visible-chars precedent. dev2 corrected: **`tail_lines` is insufficient — it walks `0..self.rows` (visible screen rows only), NOT scrollback history**. The two findings reconcile as:

- **Pattern is established**: `tail_lines` is the canonical visible-text-extraction shape (cell walk, wide-char handling, trailing-whitespace trim) — PR-3 should match this style, NOT invent a parallel one.
- **Scope is different**: `pane_snapshot` needs scrollback (lines below `Line(0)`), which `tail_lines` does not handle.
- **Resolution**: PR-3 adds `VTerm::read_scrollback(max_lines: usize) -> String` that **extends** the `tail_lines` cell-walk pattern to negative line indices via `safe_cell()` bounds-checking. Same conventions, broader range.

**Other prior-art findings retained**:

| Helper | Location | Mode | Use |
|---|---|---|---|
| `dump_screen` | `src/vterm.rs:382+` (`51503d0`) | preserve-ANSI | full state replay path |
| `tail_lines` | `src/vterm.rs:333-380` (`61044d9`) | visible-chars | AwaitingOperator / Telegram context |

Upstream consumers (`304a28d`, `df9a01f`) moved to rendered-screen semantics rather than raw-byte parsing — confirms visible-text is the established convention for human-facing observation.

**Past MCP tool addition shape**: `f448e1d` / `5bad7f3` / `3b7ccc2` — schema + dispatch + focused tests in same PR.

**`describe_instance` design**: handler at `instance.rs:200-220` (blame `37f2dd1`), tool present since `99e8590`. **No commit evidence of an explicit "forbid content" policy** — pragmatic metadata scope. PR-3's content-inclusive snapshot does not violate any prior decision.

**ANSI strip vs preserve — already chosen at codebase scale**:
- preserve: `dump_screen` (replay)
- visible-chars: `tail_lines` (human analysis)

For `pane_snapshot`: prior-art unambiguously says **visible-text default** (extend `tail_lines` style to scrollback), with optional raw-mode only if a concrete need surfaces. Confirms §3.4 cost-benefit recommendation.

**Revised LOC estimate** (after dev2's correction that `tail_lines` is insufficient): the new `read_scrollback` method is ~30–50 LOC (mirrors `tail_lines` shape), the MCP/API wire-up + tests bring total to **~150–220 LOC**. Lower than operator's ~300 estimate, but higher than the optimistic "just expose tail_lines" reading I had after reviewer2's first-glance signal. Operator to revise per §5.

### 3.5 Cleanest scrollback read API (dev2 ranking)

dev2 evaluated three implementation options:

- **Option A (recommended)**: new `VTerm::read_scrollback(n: usize) -> String` — walk `grid.topmost_line()` to `grid.bottommost_line()` using `safe_cell()`; same shape as `tail_lines` extended to negative line indices. Reuses proven bounds-checking; consistent with codebase patterns; no new deps.

```rust
// Sketch from dev2:
pub fn read_scrollback(&self, max_lines: usize) -> String {
    let grid = self.term.grid();
    let top = grid.topmost_line();      // Line(-history_size)
    let bot = grid.bottommost_line();   // Line(screen_lines - 1)
    let total = (bot.0 - top.0 + 1) as usize;
    let start_line = if total > max_lines {
        Line(bot.0 - max_lines as i32 + 1)
    } else {
        top
    };
    // ... same cell-walk as tail_lines ...
}
```

- **Option B**: `grid.iter_from(Point::new(topmost_line, Column(0)))` via alacritty `GridIterator`. Cleaner iteration but doesn't auto-handle wide-char spacers (must filter `WIDE_CHAR_SPACER`) or line breaks. Less consistent with existing patterns.
- **Option C**: extend `extract_text` for negative line offsets. Selection-based API; awkward for "last N lines" semantics.

**Plan recommendation**: Option A.

### 3.3 Prior-art (reviewer2)

**🎯 GAME-CHANGER**: `vterm.rs` already has the helpers we need — PR-3 should compose, not invent.

| Helper | Location | Origin | Mode |
|---|---|---|---|
| `dump_screen` | `src/vterm.rs:382+` | `51503d0` | preserve-ANSI / full state replay |
| `tail_lines` | `src/vterm.rs:333-380` | `61044d9` | **plain visible chars** — exactly what `pane_snapshot` operator-facing v1 needs |

`tail_lines` already serves AwaitingOperator / Telegram context paths. Upstream consumers (e.g., commits `304a28d`, `df9a01f`) moved to rendered-screen semantics rather than raw-byte parsing — confirming visible-text is the established convention for human-facing observation.

**Implication for §3.1 minimal-delta**: PR-3's `vterm.rs` work is just exposing `tail_lines` (or a thin wrapper over it) to the MCP boundary. Operator's ~300 LOC estimate may be high; revised estimate likely **~120–180 LOC** once the wrapper / handler / wire-up is counted (dev2 to confirm structurally).

**Past MCP tool addition shape — canonical patterns**:
- `f448e1d` (unified `send`): RED tests first, then dispatch / schema wiring, alias retention.
- `5bad7f3` (`inbox` expansion): schema extension + dispatch fan-in + legacy-alias compat.
- `3b7ccc2` (7 CRUD consolidation): `src/mcp/tools.rs` + `src/mcp/handlers/mod.rs` + routing tests, concentrated.
- Pattern summary: **schema + dispatch + focused tests in same PR**, often with migration aliases for breaking-name changes (not applicable here — `pane_snapshot` is a new name).

**`describe_instance` metadata-only design**:
- Handler at `src/mcp/handlers/instance.rs:200-220`, blame `37f2dd1`. Fetches LIST snapshot + metadata merge; no PTY content/screen included.
- Tool present since MCP-restore era (`99e8590`); split-dispatch references at `src/mcp/handlers/mod.rs:117`, `src/mcp/tools.rs:93-95`.
- **No commit evidence of an explicit "forbid content" policy** — pragmatic metadata scope, not formal security decree. PR-3's content-inclusive `pane_snapshot` does not violate any prior decision.

**ANSI strip vs preserve — already chosen at codebase scale**:
- Preserve: `dump_screen` (replay)
- Visible-chars: `tail_lines` (human analysis)
- For `pane_snapshot`: prior-art unambiguously says **visible-text default** (tail_lines-style), with optional raw-mode only if a concrete need surfaces. Confirms §3.4 cost-benefit recommendation.

### 3.4 Cost-benefit boundaries (lead2)

| Question | Recommendation | Rationale |
|---|---|---|
| ANSI: strip-only vs strip + preserve modes? | **Strip-only for v1** | alacritty cells already store final chars (strip is free). "Preserve" requires capturing raw PTY stream — separate code path, much harder. v1 use case ("see what happened") is fully served by strip. Add preserve when a concrete need surfaces. |
| Default `lines`? | **100** | Roughly one screen of context; bounded so default response stays small. |
| Max `lines`? | **scrollback size = 10000** | Matches `vterm.rs:119 scrolling_history` config. Reject larger. |
| Include cursor position / styling info? | **No** | KISS — text-only return. If needed, separate tool later. |
| Permission model? | **default-allow** per operator brief | Read-only; no destructive capability. |
| `target` resolution: instance name only, or also pane id? | **Instance name** | Matches `describe_instance` convention. `move_pane` operates on pane id, but snapshot is per-instance. |
| Pane with no PTY content yet? | **Return empty string + ok=true** | Don't error; observability tool should be safely idempotent. |
| Should tool list / metadata reflect that scrollback may include sensitive content? | **Not in v1** | Single-operator threat model — operator already sees TUI; snapshot just makes that view scriptable. |

### 3.5 §3.5.10 wire-format invariant requirement

PR-3 adds an MCP tool — `mcp/tools.rs` change is wire-format scope per §3.5.10 Sprint-30 amendment. Required:
- Update `tool_definitions_count_invariant_post_sprint_30` count
- Add a schema-shape invariant for `pane_snapshot` (e.g., `assert!(schema["properties"].as_object().unwrap().contains_key("target"))`)

### 3.6 §3.5.11 test-first requirement

Feature PR — must commit a failing test BEFORE the impl that makes it pass. Per protocol §3.5.11. PR-3 dispatch should require this in success criteria.

## 4. Cross-cutting concerns

### 4.1 Dependency order (recommended)

1. **PR-1 first** (lowest risk, smallest LOC, no invariant churn) — establishes pipeline rhythm
2. **PR-2 second** (deletion, invariant 26 → 25)
3. **PR-3 third** (addition, invariant 25 → 26, net 26 unchanged at sprint end)

Alternative: PR-1 + PR-2 in parallel, PR-3 sequentially. Both deletions are independent.

### 4.2 Files that need cross-PR attention

- `src/mcp/tools.rs:367-384` invariant count (commit `4c326a5`) — touched by PR-2 (`-1`) AND PR-3 (`+1`); each PR commits the value relevant at its end-state.
- `docs/ARCHITECTURE-QUICK-START.md`:
  - "26 MCP tools" claim is correct at sprint-end (PR-2 −1 + PR-3 +1 = 26)
  - **Stale reference**: doc cites `tests/mcp_tools_count.rs` (added in `ac5437f`) but that file does not exist. Per reviewer2 archaeology — `git log --diff-filter=A` and `--diff-filter=D` both empty for the path. Real invariant lives at `src/mcp/tools.rs:367-384`. Fix the reference in whichever PR lands first that touches the count.
- `docs/architecture.md`, `docs/decisions/`, MEMORY mentions of `tool_kill` — sweep in PR-2.

### 4.3 Test-impact estimate (post-dev2 confirmation)

- `tests/mcp_roundtrip.rs:144` uses `tools.len() >= 25` (≥ comparator) — passes at 25 post-PR-2; no change needed.
- `tests/mcp_characterization.rs` and `tests/mcp_proxy_*.rs` — confirmed by dev2 to have **no `tool_kill` references**. Clean.
- New tests required for PR-3:
  - `vterm.rs` inline test for `read_scrollback` (per §3.5.11 test-first)
  - `mcp/handlers/instance.rs` inline test for `handle_pane_snapshot` (target not found, valid target, lines parameter bounds)
  - integration test in `tests/` for the MCP wire path (per §3.5.10 wire-format scope)
  - tool count invariant assertion update

### 4.4 Tier-B label clarification (operator decision needed — §5)

Operator's brief tagged this dispatch as **"Tier-B"**. reviewer2's repo grep finds only `Tier-1` / `Tier-2` taxonomy in `docs/FLEET-DEV-PROTOCOL-v1.md:521-557` (review tiers). **The string "Tier-B" does not appear in repo docs or source**. This is novel terminology in the operator brief and should be confirmed:

- Possibility A: operator means a new informal "B-tier risk" (between low LOW docs-only path and high cross-vantage dual-review). Map it explicitly so future dispatches don't drift.
- Possibility B: operator typo / shorthand for "Tier-2"-level review with structural focus, not threat-model focus.
- Possibility C: a label maintained in the operator's mental model not yet codified.

**Plan recommendation**: surface as decision point §5 (5) — request operator define Tier-B in protocol or map to existing tier so the impl wave dispatches can cite it correctly.

## 5. §13 decisions surfaced for operator (post-PLAN merge)

These require operator decisions before impl wave dispatches. Numbers reflect dev2 + reviewer2 input (not pre-confirmation guesses).

1. **LOC budget acceptance / revision**:
   - PR-1 ≈ **10–20** LOC (one format string + signature drop + test input update). Operator estimate (~50) was high.
   - PR-2 ≈ **120–170** LOC removed (8-file table in §2.2 + 4 tests + comments). Operator estimate (~150) is in-range.
   - PR-3 ≈ **150–220** LOC added (new `read_scrollback` + tool registry + handler + API method + tests + invariant). Operator estimate (~300) was high but not as low as my interim "tail_lines reuse" suggested. **dev2's correction**: `tail_lines` is visible-screen-only; scrollback needs new method.
2. **Dependency order**: confirm `PR-1 → PR-2 → PR-3` recommendation, or re-order.
3. **Sprint placement**: confirm all three in Sprint 33, or split (e.g., PR-3 to Sprint 34 if pane-state-classifier rework on operator's branch conflicts with vterm exposure).
4. **Impl wave dispatch sequencing**: single agent serial, parallel per PR, or impl-1/impl-2 split per perspective?
5. **Tier-B label** (per §4.4): define in protocol or map to existing Tier-1 / Tier-2.
6. **Scope inclusions in PR-2** (recommend YES on each):
   - §2.4 (a) `supports_fg_pgid` field removal (confirmed dead — dev2 + reviewer2 agree)
   - §2.4 (c) `src/health.rs:52, :210` doc-comment updates
   - §2.4 (g) `docs/ARCHITECTURE-QUICK-START.md` stale `tests/mcp_tools_count.rs` reference fix
7. **Scope NON-inclusions** (revised per dev2 — keeping out of PR-2):
   - `verify_tcgetpgrp` in `backend_harness.rs` — used by behavioural tests, not tool_kill-specific. **KEEP** (revision of my earlier §2.4 (b) recommendation).
8. **PR-3 ANSI mode**: confirm strip-only (visible-text) for v1 per §3.4 + §3.3 prior-art convergence.

## 6. Process notes

- **Worktree**: `/Users/suzuke/.agend-terminal/workspace/lead2/repo` on branch `plan/sprint33-ui-observe-2026-04-29` off `84a6e21`
- **Decision**: `d-20260429101041254472-0`
- **Fleet task**: `t-20260429101043734307-0`
- **Dispatches**:
  - dev2 (kiro-cli) — structural — dispatched 2026-04-29T10:11Z, reported 2026-04-29T10:17Z (~6 min wall)
  - reviewer2 (codex) — prior-art — dispatched 2026-04-29T10:11Z, reported 2026-04-29T10:15Z (~4 min wall, with header-then-body re-send round-trip due to MCP `request_kind=query` field-name bug — see PR #315 §5)
- **PR path**: §3.5.5 LOW docs-only single-reviewer self-merge per operator authorisation. lead2 owns `watch_ci`. Verdict mirrors per §3.5.13.
- **Scope freeze**: operator decisions in §0 are not 4-perspective targets.
- **Tier-B**: no trait validation; daemon-side feature work only.
