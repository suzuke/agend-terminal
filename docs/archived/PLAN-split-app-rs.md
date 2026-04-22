# Plan: decompose src/app.rs and backfill layout.rs / state.rs tests

> **Status: SHIPPED** — landed on `main` (commits `726562e`, `88a0d74`, 2026-04-18). Doc retained for historical/provenance.

> **Status: DONE 2026-04-18** — landed on `main` via commits `726562e` (PRs 1-4 + layout tests) and `88a0d74` (PRs 5-9). All 9 planned submodules exist under `src/app/` (mod.rs + session.rs + commands.rs + dispatch.rs + mouse.rs + overlay.rs + pane_factory.rs + telegram_hooks.rs + tui_events.rs + api_server.rs); `src/app/mod.rs` is ~700 LOC (down from 2,552). Kept below as historical design record.

Worktree: `/Users/suzuke/Documents/Hack/agend-refactor-split-app`
Branch:   `refactor/split-app-rs`

## 0. Current-state measurements (verified, not assumed)

| File              | LOC   | pub items | `#[test]` | Coverage density |
| ----------------- | ----- | --------- | --------- | ---------------- |
| src/app.rs        | 2,552 | 3 (+3 `pub(crate)`) | 0 | 0 / KLOC |
| src/layout.rs     | 1,263 | 14        | 11        | 8.7 / KLOC       |
| src/state.rs      |   594 |  3 + ~9 methods | 21  | 35.4 / KLOC |
| src/keybinds.rs   |   197 |  2        |  0        | 0                |
| tests/integration |   302 |  —        |  6        | subprocess       |
| tests/mcp_roundtrip|  408 |  —        | 10        | subprocess       |

External consumers of `app::` symbols:
- `src/main.rs:286` — `app::run(fleet.as_deref())`
- `src/render.rs:4` — `use crate::app::MenuItem`
- `src/api.rs` — `TuiEventSender`, `TuiEvent`, `LayoutHint::parse_hint`

Everything else in app.rs is private and free to move.

`app.rs` hot-spots by function length (LOC):
- `run_app` — L231–L942 = **712 LOC** (event loop god)
- `execute_command` — L1493–L1766 = **274 LOC**
- `restore_with_reconciliation` + `restore_node_reconciled` — L1870–L2076 = **207 LOC**
- `handle_mouse_selection` — L2442–L2544 = **103 LOC**
- `handle_team_created` — L2186–L2278 = **93 LOC**
- `build_menu_items` — L990–L1039 = **51 LOC**

## 1. Decomposition blueprint — `app/` submodule tree

Target post-refactor shape (total ≈ 2,552 LOC split across 9 files of 80–350 LOC each):

```
src/app/
├── mod.rs                (~250 LOC) — pub fn run(), public types, re-exports
├── session.rs            (~260 LOC) — Session/SessionTab/SessionNode/SessionPane,
│                          save_session, save_all_session_ids, sync_fleet_yaml,
│                          restore_with_reconciliation, restore_node_reconciled
├── pane_factory.rs       (~250 LOC) — create_pane, create_pane_from_resolved,
│                          attach_pane, spawn_pane_tab, resolve_backend,
│                          unique_fleet_name
├── overlay.rs            (~350 LOC) — Overlay enum, CloseTarget,
│                          handle_list_scroll, all overlay key handling
│                          (NewTabMenu / SplitMenu / RenameTab / RenamePane /
│                           ConfirmClose / TabList / Help / Scroll /
│                           Command / Decisions / Tasks)
├── commands.rs           (~300 LOC) — execute_command + command parsing
├── dispatch.rs           (~250 LOC) — Action → side-effect handler
│                          (the big match arm in run_app L599–L747)
├── mouse.rs              (~200 LOC) — tab_bar_hit_test, handle_mouse_selection,
│                          border_drag state machine, TabBarClick, copy_to_clipboard
├── tui_events.rs         (~200 LOC) — TuiEvent + LayoutHint (pub(crate)),
│                          handle_tui_event + handle_instance_created/deleted,
│                          handle_team_created + remove_agent_pane
├── api_server.rs         (~80 LOC)  — ApiGuard + start_api_server + auto_start_fleet
└── telegram_hooks.rs     (~80 LOC)  — maybe_create_telegram_topic,
                          maybe_delete_telegram_topic, telegram_status_from_config
```

### Justification per submodule (size + minimum API surface back to mod.rs)

**session.rs** — Serde types + save fns + reconciliation. Pure data, only touches `Layout` + disk. No shared `&mut` spaghetti with overlay/mouse. API: `save_session(&Path, &Layout)`, `save_all_session_ids(&Path, &Layout)`, `sync_fleet_yaml(&Path, &Layout)`, `restore_with_reconciliation(...) -> bool`.

**pane_factory.rs** — Six fns sharing `spawn_agent → subscribe → wrap VTerm → spawn forwarder thread` recipe. API: six `pub(super) fn`s.

**overlay.rs** — `Overlay` enum + all 11 per-variant key handlers currently inlined in `run_app`. Owns modal state; receives `KeyEvent`; returns `OverlayOutcome`. API: `pub(super) fn handle_key(overlay: &mut Overlay, key: KeyEvent, ctx: &mut OverlayCtx) -> OverlayOutcome`.

**commands.rs** — 274 LOC of cmd-string parsing. API: `pub(super) fn execute(cmd: &str, ctx: &mut CommandCtx) -> bool`.

**dispatch.rs** — Post-overlay `Action` match (keybinds.rs already yields `Action`; the switch is missing). API: `pub(super) fn dispatch(action: Action, ctx: &mut DispatchCtx) -> DispatchResult { needs_resize, new_overlay, should_break }`.

**mouse.rs** — Self-contained once `border_drag`, `dragging_pane`, `selecting_pane` are threaded in. API: `pub(super) fn handle(mouse: MouseEvent, layout: &mut Layout, overlay_active: bool, state: &mut MouseState) -> MouseOutcome`.

**tui_events.rs** — Cohesive (TuiEvent family + 4 handlers). Must re-export `TuiEvent`, `LayoutHint` at `mod.rs` because `api.rs` imports them.

**api_server.rs** — `ApiGuard` + `start_api_server` + `auto_start_fleet`. Private.

**telegram_hooks.rs** — Three `maybe_*` fns — "fire-and-forget background thread" pattern.

### Final `mod.rs` skeleton

```rust
mod api_server;
mod commands;
mod dispatch;
mod mouse;
mod overlay;
mod pane_factory;
mod session;
mod telegram_hooks;
mod tui_events;

pub use overlay::MenuItem;                // render.rs dep
pub(crate) use tui_events::{TuiEvent, TuiEventSender, LayoutHint};  // api.rs deps

pub fn run(fleet_path_override: Option<&str>) -> Result<()> { ... }
fn run_app(terminal: &mut DefaultTerminal, fleet_override: Option<&Path>) -> Result<()> {
    // ~120 LOC of orchestration only — no overlay body, no action body,
    // no mouse body. Reads like the narrative it should be.
}
```

## 2. Safe extraction order (each step compiles + tests pass)

1. **`session.rs`** — *lowest risk*. Serde + two save fns + reconciliation. No shared `&mut` with overlay/mouse. `run_app` loses ~260 LOC.
2. **`telegram_hooks.rs`** — three pure wrappers. Trivial. Loses ~80 LOC.
3. **`api_server.rs`** — `ApiGuard` + two fns. Already private, zero shared state. Loses ~80 LOC.
4. **`pane_factory.rs`** — 6 `create_pane*`/`attach_pane`/`spawn_pane_tab`. Many call sites but signatures stable. Loses ~250 LOC.
5. **`tui_events.rs`** — move `TuiEvent`, `LayoutHint`, `handle_tui_event` + 3 sub-handlers. Touches `api.rs` — keep re-export in `mod.rs`.
6. **`overlay.rs`** — moderate. Define small return-value protocol (`OverlayOutcome`) to replace direct mutation of `layout`/`needs_resize` from inside `run_app`. Move the enum + `handle_list_scroll` first, then migrate one overlay arm per commit.
7. **`commands.rs`** — moderate. `execute_command` touches `layout`, `registry`, `name_counter`, `telegram_state` — bundle into `CommandCtx<'_>`.
8. **`mouse.rs`** — higher. `border_drag` is a local `Option<(...)>` inside `run_app`; becomes a field of `MouseState`.
9. **`dispatch.rs`** — *highest*. The giant `Action` match has 35+ arms, many mutating `last_tab`/`overlay`/`needs_resize`/`layout`, occasionally `break`. Introduce `DispatchResult { needs_resize, new_overlay, should_break }`.

After step 9, `run_app` shrinks 712 LOC → ~120 LOC of orchestration.

## 3. Unit-test gap list

### 3a. layout.rs gaps (currently 11 tests — border/ratio math only)

Public functions with zero coverage:

**`PaneNode`**:
- `pane_ids` / `find_pane` / `find_pane_mut` — single leaf, 2-leaf split, nested 4-leaf, nonexistent id.
- `first_pane` — single leaf, deep-left leaf.
- `pane_count` / `agent_count` — all-shells, mixed, all-agents.
- `has_notification` / `agent_names` / `has_agent` / `find_pane_id_by_agent` — none/some/duplicate-name (duplicate-name is a bug magnet: `find_pane_id_by_agent` returns first hit — test documents that).

**`Tab`** (highest bug magnet since all mutation enters here):
- `cycle_focus` — single pane (no-op), 3 panes (wraps).
- `focus_direction` — `pane_rects` empty (fallback via `rem_euclid`), 2×2 grid (best overlap), wrap-around.
- `split_focused` — success, focus-id not in tree (remaining pane returned).
- `close_focused` / `close_pane_by_id` — last pane returns None, close focused advances focus_id to sibling, close non-focused keeps focus.
- `apply_layout` — 1 pane (no-op, only sets `last_layout`), 3 panes → each of 5 presets, verify `pane_rects.clear()`.
- `next_layout` — from None starts at EvenHorizontal, full cycle.
- `title_bar_at` — exact hit, miss on agent-state suffix, CJK width, zoomed (currently no special handling — document).
- `pane_at` — hit/miss.
- `clear_drag` / `clear_transient_input` — fields reset.

**`Layout`**:
- `next_pane_id` — monotonic, starts at 0.
- `add_tab` — active advances; outgoing `clear_transient_input` called (observable via `selecting_pane = None`).
- `next_tab`/`prev_tab` — empty (no-op), wrap at ends.
- `goto_tab` — out-of-bounds no-op, valid switches.
- `close_tab` — oob None, close active shifts active back, close last keeps active at 0.

**Free functions**:
- `swap_panes` — same id, nonexistent id, two leaves, across splits.
- `resize_focused` — Up/Down/Left/Right, finds ancestor split of correct axis, clamps via ratio bounds, no ancestor (single-pane tab).
- `flatten_tree_into` — preserves left-to-right order.
- `build_preset` — 2/3/4/5 panes × 5 presets, pane_count preserved.

**`LayoutPreset`**: `next` full cycle, `from_name` aliases (`even-h`, `main-v`, `tile`), `all_names` format.

Priority (by bug-magnet potential):
1. `Tab::close_focused` / `close_pane_by_id`
2. `Tab::apply_layout` + `build_preset`
3. `swap_panes`
4. `Tab::focus_direction` fallback-path branch
5. `resize_focused`

### 3b. state.rs gaps (currently 21 tests, strong on hysteresis + Claude patterns)

14-state transition matrix — covered / uncovered pairs that `feed`/`transition` can actually reach:

| From \ To        | Ready | Idle | ToolUse | Thinking | PermPrompt | ContextFull | RateLimit | UsageLimit | AuthError | ApiError | Crashed | Restarting | Hang |
| ---------------- | :---: | :--: | :-----: | :------: | :--------: | :---------: | :-------: | :--------: | :-------: | :------: | :-----: | :--------: | :--: |
| Starting         |  ✓    |  ·   |    ·    |    ·     |     ·      |      ·      |     ·     |     ·      |     ·     |     ·    |    ·    |     ·      |  ·   |
| Ready            |  —    |  ·   |    ·    |    ·     |     ·      |      ·      |     ·     |     ·      |     ·     |     ·    |    ·    |     ·      |  ·   |
| Idle             |  ✓    |  —   |    ·    |    ✓     |     ·      |      ·      |    ✓      |     ·      |     ✓     |     ·    |    ·    |     ·      |  ·   |
| ToolUse          |  ·    |  ·   |    —    |    ·     |     ·      |      ·      |     ·     |     ·      |     ·     |     ·    |    ·    |     ·      |  ·   |
| Thinking         |  ·    |  ✓   |    ·    |    —     |     ✓      |      ✓      |     ·     |     ·      |     ·     |     ·    |    ·    |     ✓      |  ·   |
| PermissionPrompt |  ·    |  ·   |    ·    |    ·     |     —      |      ·      |     ·     |     ·      |     ·     |     ·    |    ·    |     ·      |  ·   |
| ContextFull      |  ·    |  ·   |    ·    |    ·     |     ·      |      —      |     ·     |     ·      |     ·     |     ·    |    ·    |     ·      |  ·   |
| RateLimit        |  ·    |  ✓   |    ·    |    ·     |     ·      |      ·      |     —     |     ·      |     ·     |     ·    |    ·    |     ·      |  ·   |
| UsageLimit       |  ·    |  ·   |    ·    |    ·     |     ·      |      ·      |     ·     |     —      |     ·     |     ·    |    ·    |     ·      |  ·   |
| AuthError        |  ·    |  ·   |    ·    |    ·     |     ·      |      ·      |     ·     |     ·      |     —     |     ·    |    ·    |     ·      |  ·   |

Covered pairs: **9**. Most intra-active and active↔error edges uncovered.

Highest-value missing tests:
1. **Active → error** (Thinking/ToolUse → RateLimit/UsageLimit/AuthError/ContextFull): instant transition even when active hold is only 0.1s.
2. **Error → recovery**: RateLimit→Idle covered; add UsageLimit→Idle, AuthError→Idle, ContextFull→Idle — each needs 2s active hold + state_buf clear.
3. **ToolUse transitions** — none currently. Idle→ToolUse (higher prio instant), ToolUse→Thinking (same prio, active hold), ToolUse→Idle (lower, 2s hold).
4. **Restarting** — `set_restarting` tested; no test for `Restarting→Ready` recovery via feed.
5. **Hang** — zero transitions tested; `Hang` priority=1 exists but `transition()` doesn't fire it — `HealthTracker::check_hang` gates it externally. Document the gap.

Per-function missing tests:
- `priority()` — exhaustive match across 14 states (guards against enum reorder).
- `is_unavailable()` — Crashed/Restarting true; else false.
- `display_name()` — round-trip; ensures snake_case stays stable for serialization.
- `StatePatterns::for_backend(Backend::{Codex, OpenCode, Gemini, KiroCli})` — at least one `detect` happy-path per backend (today only Claude tested — 20% coverage).

Test cases per function (normal / edge / error):
- `priority`: each state returns documented value (14 asserts); boundary via `is_error` (Thinking not error, ContextFull error, Restarting error).
- `is_unavailable`: Crashed, Restarting true; Idle, Starting false.
- `StatePatterns::detect`: match / no match / multiple matches returns highest-priority (Codex: "429 rate limit" → RateLimit not Idle).
- `StateTracker::feed`: empty (tested), UTF-8 boundary at 2048-byte truncation (risk zone — char_boundary logic), trigger pattern at very end of buf, trigger after 2KB garbage.
- `transition`: all 4 branch combos — error instant, higher-prio instant, passive→lower < 5s (no), passive→lower > 5s (yes), active→lower < 2s (no), active→lower > 2s (yes), same-state no-op (tested).

Target: +25–30 tests, lifting state.rs density from 35/KLOC → ~85/KLOC, covering 11 more transition pairs.

## 4. Validation strategy

### Load-bearing status of existing tests for app/layout logic

- `tests/integration.rs` (6) — spawns `agend-terminal daemon`; hits TCP. Exercises `daemon.rs`/`api.rs`/`agent.rs`/`health.rs`. **Never loads `app::run`**. Regression signal for this refactor: **zero**.
- `tests/mcp_roundtrip.rs` (10) — spawns `agend-terminal mcp`. Exercises `mcp/handlers.rs`. **Never loads `app::run`**. Signal: zero for our refactor.
- `src/layout.rs` inline tests — load-bearing for border/ratio math. **Keep green at every step.**
- `src/state.rs` inline tests — hysteresis + pattern regressions. Keep green.

### Proposed golden test before refactor (high leverage)

Add `src/app/session_tests.rs` *before* decomposition begins. `Session` serde round-trip + `restore_with_reconciliation` is the most-mutated, least-covered logic we're moving. Golden test:

1. Construct a 3-tab layout with one split-right in tab 2; save via `save_session` to tempdir.
2. Parse back with `restore_with_reconciliation` using a stub fleet config.
3. Assert (a) tab count, (b) pane count per tab, (c) split direction + ratio round-trip, (d) fleet names placed vs unplaced.

Without agent-spawn plumbing, use a `create_pane_stub` helper gated `#[cfg(test)]` skipping real `spawn_agent`.

### Defensive checks per extraction PR
- `cargo clippy --all-targets -- -D warnings`
- Manual smoke: `cargo run -- app` → create tab → split → zoom → rename → close. Document in PR body.

### Why not a TUI E2E test?
Feasible but expensive: a pseudo-terminal driver (`vt100`/`expectrl`) around the binary would validate the event loop, but the crossterm thread + crossbeam select make deterministic E2E hard. Defer. Inline unit tests on extracted modules give 80% safety at 20% cost.

## 5. First PR scope (smallest high-value slice)

**Title**: `refactor(app): extract session save/restore + add 8 layout unit tests`

**Changes**:

A. Rename `src/app.rs` → `src/app/mod.rs`. No code edits. `main.rs` / `render.rs` compile unchanged because module path is identical. **Verify**: `cargo build` green.

B. Create `src/app/session.rs`. Move out of `mod.rs`:
   - `struct Session`, `SessionTab`, `SessionNode`, `SessionPane`, `fn default_ratio`
   - `fn save_session`, `fn save_node`
   - `fn save_all_session_ids`
   - `fn sync_fleet_yaml`
   - `fn restore_with_reconciliation`
   - `fn restore_node_reconciled`
   Expose as `pub(super) fn save_session(...)` etc. `mod.rs` calls `session::save_session(&home, &layout);`. Net: `mod.rs` shrinks ~260 LOC.

C. Add `src/layout.rs` tests (inside existing `#[cfg(test)] mod tests`):
   1. `pane_count_and_agent_count_across_split` — mixed shell+agent panes
   2. `close_focused_updates_focus_to_sibling`
   3. `close_pane_by_id_returns_none_when_last`
   4. `cycle_focus_wraps_around_three_panes`
   5. `apply_layout_even_horizontal_preserves_pane_count`
   6. `next_layout_cycles_from_none_to_even_horizontal`
   7. `layout_next_tab_wraps_at_boundary`
   8. `swap_panes_across_nested_split`

   Add test helper `fn leaf(id: usize, name: &str) -> Pane` that constructs a `Pane` with dummy VTerm + dropped-sender rx (layout tests never read `rx`).

**Why this first?**
- Visible value: session-file logic (most likely to silently lose tabs on restart) gets a dedicated file, earning a golden test next.
- Shrinks `mod.rs` ~10% in one commit — publishes momentum.
- Adds **8 unit tests** for the riskiest untested layout APIs (close + apply_layout + swap_panes).
- Touches **zero public signatures** beyond intra-crate moves — no downstream (render.rs, api.rs) churn.
- Single reviewable PR (~350 LOC diff, ~80 LOC added tests).

Follow-on PRs: (2) `telegram_hooks.rs`, (3) `api_server.rs` + `auto_start_fleet`, (4) `pane_factory.rs`, (5) `tui_events.rs`, (6) `overlay.rs`, (7) `commands.rs`, (8) `mouse.rs`, (9) `dispatch.rs`. Between PRs 2–9, a dedicated PR adds **+25–30 state.rs tests** so state coverage grows in parallel with app.rs shrinking.

## Appendix: numbers backing the claims

```
wc -l src/app.rs        → 2552
wc -l src/layout.rs     → 1263
wc -l src/state.rs      →  594
wc -l src/keybinds.rs   →  197
```

Function lengths inside app.rs (line-range arithmetic from the source):
- `run_app` L231–L942 = 712
- `execute_command` L1493–L1766 = 274
- `restore_with_reconciliation` + `restore_node_reconciled` L1870–L2076 = 207
- `handle_mouse_selection` L2442–L2544 = 103
- `handle_team_created` L2186–L2278 = 93
- `build_menu_items` L990–L1039 = 51

Test counts:
- src/layout.rs → 11
- src/state.rs  → 21
- src/app.rs    → 0
