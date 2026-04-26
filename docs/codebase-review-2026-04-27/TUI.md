# Sprint 20 Track C — TUI render + keybinds + state codebase audit

**Audit metadata** (per v1.2 §3 `audit_mode=codebase_audit`)
- `reviewed_head`: `1485e85eab70ceeb43d794ecb586ee0b72d0bf04` (origin/main at audit start)
- `scope_source`: dispatch task `t-20260426210738528174-10` + scope freeze decision `d-20260426210724891457-5`
- `audit_mode`: `codebase_audit` (Sprint 20 4-track codebase review, audit-only, 0 code change)
- `audit_tier_distribution`:
  - Tier-1 hot (~70%): `src/render.rs` (2213 lines), `src/state.rs` (2386 lines), `src/keybinds.rs` (374 lines)
  - Tier-2 control: `src/layout.rs` (2106 lines), `src/app/dispatch.rs` (330 lines), `src/app/overlay.rs` (1195 lines)
  - Tier-3 peripheral: `src/vterm.rs` (718 lines), `src/app/session.rs` (395 lines)
- `sampled_commands` (top 10 most informative):
  - `rg -n "\.unwrap\(\)|\.expect\(|panic!" src/render.rs src/layout.rs src/keybinds.rs src/state.rs src/vterm.rs src/app/`
  - `rg -n "area\.height \-|area\.width \-|height \- 2|width \- 4" src/render.rs`
  - `rg -n "saturating_|checked_" src/render.rs`
  - `rg -n "as u16|as i16|as usize" src/render.rs`
  - `rg -n "\.expect\(" src/layout.rs`
  - `rg -n "lock_state|lock_registry|\.lock\(\)" src/render.rs`
  - `rg -n "deserialize|from_str|serde_json" src/app/session.rs`
  - `rg -n "fn handle_key|fn dispatch_prefix|fn handle" src/app/overlay.rs src/keybinds.rs`
  - Full read of `src/keybinds.rs` (1-374) and `src/app/session.rs` (1-340)
  - Sectioned reads of `src/render.rs` (820-960, 49-85)
- `files_touched_in_audit`: `src/render.rs`, `src/layout.rs`, `src/keybinds.rs`, `src/state.rs`, `src/vterm.rs`, `src/app/dispatch.rs`, `src/app/overlay.rs`, `src/app/session.rs`

> **Acknowledge: this is a first-sweep audit, exhaustiveness deferred to sub-tracks if needed.** Track C scope is ~13,000 lines (render.rs 2213 + layout.rs 2106 + state.rs 2386 + keybinds.rs 374 + vterm.rs 718 + app/ 5328). 2h hard cap necessitated grep-first + targeted-read methodology, not line-by-line exhaustive trace. Findings list captures patterns surfaced by grep + concentrated reads; subtle line-level bugs in unread sections are possible. Recommend follow-up sub-tracks if any HIGH finding here surfaces additional pattern instances elsewhere.

---

## Findings

### Critical

(none observed — audit complete)

No trust-boundary violations, no privileged-data leaks, no UB / unsound patterns. Track C is presentation/state layer; security-relevant surfaces (auth/crypto/handler input validation) live in Tracks A (channel) / D (MCP).

### High

**H1 — Unbounded `area.height - N` / `area.width - N` subtraction in render.rs overlay sites (5 occurrences)**

`src/render.rs` has explicit precedent for saturating subtraction in `render_menu` (lines 826-832), with an inline comment documenting the failure mode:
```rust
// Line 823-825 (render_menu — fixed):
// `items.len() as u16` can silently truncate; `area.height - 2` panics
// if height < 2. Use saturating arithmetic throughout so a tiny
// terminal renders a (clipped) menu instead of underflow-panicking.
let item_count = u16::try_from(items.len()).unwrap_or(u16::MAX);
let menu_height = item_count.saturating_add(4).min(area.height.saturating_sub(2));
let menu_width = 50u16.min(area.width.saturating_sub(4));
```

But the same pattern is **not applied** at five other sites:

| Line | Function | Code | Trigger |
|---|---|---|---|
| 618-619 | `border_char` (called from `render_pane`) | `area.x + area.width - 1`, `area.y + area.height - 1` | Pane width or height = 0 |
| 867 | `render_rename` | `area.width - 4` | Terminal width < 4 |
| 896 | `render_tab_list` | `area.height - 2` | Terminal height < 2 |
| 897 | `render_tab_list` | `area.width - 4` | Terminal width < 4 |
| 955 | `render_move_pane_target` | `area.height - 2` | Terminal height < 2 |
| 956 | `render_move_pane_target` | `area.width - 4` | Terminal width < 4 |
| 1020 | `render_confirm` | `area.width - 4` | Terminal width < 4 |

**Failure mode**: u16 underflow in debug → panic; in release → wrap to 65534+. Either way, TUI crashes or renders to absurd sizes.

**Trigger conditions**: User shrinks terminal window below the threshold (e.g. 2-row tall) while one of these overlays is active. `render_tab_list` (Ctrl+B w) and `render_move_pane_target` (Ctrl+B !) are user-triggered modes; `render_rename` and `render_confirm` are also operator-triggered.

**Same class as PR #194 vterm OOB hotfix** (HOTFIX vterm.rs:140-143 capped render loop by `grid.screen_lines()` + `grid.columns()`). Operator already burned by this pattern once.

**Fix**: 1 helper, 5 call sites:
```rust
fn clamp_overlay_size(area: Rect, base: u16, w_pad: u16, h_pad: u16) -> (u16, u16) {
    let h = base.saturating_add(4).min(area.height.saturating_sub(h_pad));
    let w = base.min(area.width.saturating_sub(w_pad));
    (w, h)
}
```
Replace each `area.height - N` / `area.width - N` with saturating_sub. Estimated diff: ~30 lines, sub-30-min implementation.

**Severity**: HIGH. Crash-class for narrow path (rare but reproducible), and we have prior incident (PR #194) on identical pattern.

### Medium

**M1 — `layout.rs` heavy `.expect("root is always Some")` invariant (7 sites)**

`Tab.root: Option<PaneNode>` is functionally non-empty (`Tab::new`, `Tab::with_root`, all constructors require a root). The `Option` exists only to support the `take()` + `replace()` mutation pattern for tree restructuring. As a consequence, 7 sites unwrap with `expect("root is always Some")`:

| Line | Context |
|---|---|
| 900 | `root()` getter |
| 904 | `root_mut()` getter |
| 1012 | `swap_into` |
| 1037 | `flatten_to_singleton` |
| 1113 | `restore_from_swap` |
| 1140 | `replace_root` |
| 1307 | `move_pane_across_tabs` (source-side take) |

**Risk**: Each `.take()` opens a brief window where `root` is `None`. If a future refactor introduces a panic between `take()` and the corresponding assignment (e.g. inside one of the helper fns called between them), `root` stays `None` and any subsequent `.expect()` panics + corrupts the entire TUI session.

**Defensible today**: All seven sites follow `let root = self.root.take().expect(...); ... self.root = Some(new_root);` patterns with no panicky calls between them. Static analysis (rustc + tests) catches obvious panics.

**But**: Rust idiom for non-empty Option is `mem::replace(&mut self.root, sentinel)` with a non-empty placeholder, OR refactor to `Tab.root: PaneNode` direct + use a tree-internal swap mechanism. Both eliminate the brittle invariant.

**Severity**: MEDIUM (works today, fragile to refactor, large blast radius if invariant breaks).

**M2 — keybinds.rs Capital `'S'` inconsistency (line 196)**

Already filed as backlog `t-20260426122506967277-9` (PR-AT polish followup). Calling out so this audit's coverage is honest:
```rust
// Line 194-198:
KeyCode::Char('D') => Action::ShowDecisions,
KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::SHIFT) => Action::ShowDecisions,
KeyCode::Char('s') => Action::ShowStatus,                      // ⚠️ only lowercase
KeyCode::Char('m') | KeyCode::Char('M') => Action::ShowMonitor,
KeyCode::Char('T') | KeyCode::Char('t') => Action::ShowTasks,
```
`'D'` / `'M'` / `'T'` cover both cases. `'s'` does not — `Ctrl+B Shift+s` (Kitty-protocol terminals) and `Ctrl+B S` (legacy capital) both fall through to `Action::None`. One-word fix.

**Severity**: MEDIUM (UX inconsistency, no crash). Backlog already exists.

### Low

**L1 — dispatch criterion 3 mismatch with code organization (Path-keyword auto-Critical rule)**

Dispatch criterion 3 stated: *"state.rs session restoration / trust boundary → auto-Critical"*. Reading `src/state.rs` confirms it is a **PTY output state classifier** (regex pattern matching against vterm screen text), **not** a session restoration / trust boundary module. Session restoration is in `src/app/session.rs`.

I applied the auto-Critical rule to `app/session.rs` instead. Result: `session.rs` deserializes `session.json` as a layout-only hint (`SessionPane.fleet_instance_name` is a lookup key into fleet.yaml). **Trust model**: attacker controlling `session.json` already controls `fleet.yaml` (same `$AGEND_HOME` directory permissions); no privilege escalation surface. Acceptable. Calling out so the dispatch criterion 3 wording can be corrected for future audits.

**Severity**: LOW (audit-process artifact, no code defect).

**L2 — `render_menu` warning comment fixes one site, not the pattern**

`render_menu` line 823-825 comment explicitly documents the `area.height - 2 panics` failure mode and uses saturating arithmetic. But the same comment author's adjacent sites (`render_tab_list`, `render_move_pane_target`, `render_confirm`, `render_rename`) don't apply the same fix. This is **per-site fix without pattern propagation** — exactly the antipattern that produces H1's recurring vulnerability. Refactor recommendation in §Refactor would close the loop.

**Severity**: LOW (style/maintenance — not a crash itself, but enables H1 to recur).

---

## Praise

### Patterns worth replicating

- **`src/keybinds.rs` PrefixState machine** (lines 56-139). `PrefixState::Normal` / `WaitingFirst` / `Repeat { since: Instant }` is a clean three-state design with explicit `REPEAT_TIMEOUT = 1500ms`. Repeat-mode auto-exits on timeout or Enter/Esc. Each chord conflict (Shift+H/J/K/L vs FocusXxx, Shift+D vs Detach, Kitty vs legacy uppercase) has paired unit tests (lines 246-348). **This is the gold standard for input-mode state machines in this codebase.**
- **`src/state.rs` hash-based dedup in `feed()`** (line 542+ `StateTracker`). Skips silence-timer bumps and pattern detection when the screen text hash matches the previous snapshot. Prevents invisible terminal chatter (cursor blinks) from resetting hang/awaiting timers. Subtle correctness fix that's invisible in normal operation; documented inline.
- **`src/render.rs` `render_menu` saturating-arithmetic with explicit failure-mode comment** (lines 823-832). Even though the pattern wasn't propagated (see L2), the local fix and inline rationale ("a tiny terminal renders a (clipped) menu instead of underflow-panicking") is a correct, didactic example. Replicating to the four other sites makes Track C panic-free under terminal-resize stress.
- **`src/vterm.rs` post-PR #194 grid-bounded render loop** (lines 140-143). Three-level `.min()` chain caps render bounds to alacritty grid actual dimensions. Defensive guard documented inline. Operator already verified the fix unblocks production.

### Impressive complexity (audit visibility, not for replication)

- **`src/layout.rs` Tab/PaneNode tree with take/replace mutation** (M1). The tree restructuring code (split, move, swap, flatten) is correct but built on a non-empty-Option invariant. It works today; a future refactor breaking the invariant would crash the entire TUI. Replicating the pattern is a footgun; understanding it is required for any layout work.

### Sub-bucket: documentation quality

- **`src/state.rs` module-level doc comment** (lines 1-15) is unusually thorough. Explains the pattern-vs-screen-text design choice, hysteresis policy (instant for errors, 2s active, 5s passive), and dedup rationale. **Other modules should adopt this density.**
- **`src/keybinds.rs:9-10` `REPEAT_TIMEOUT` constant** has explicit semantic justification ("repeat mode stays active after a repeatable key"). Same for **`PrefixState` enum doc comments** (lines 57-64).

---

## Coverage

### Tier-1 (deep dive — ~70% of audit time)

- `src/render.rs` (2213 lines) — function inventory via rg, hot-pattern grep, sectioned read of overlay rendering (lines 820-960) and main render entrypoint (lines 49-85). **Findings: H1, L2.**
- `src/state.rs` (2386 lines) — module doc + AgentState enum read (lines 1-130). Confirmed scope is PTY classifier, not session restoration. **Findings: L1 (dispatch criterion mismatch).**
- `src/keybinds.rs` (374 lines) — full read. **Findings: M2.**

### Tier-2 (walkthrough)

- `src/layout.rs` (2106 lines) — function inventory + grep for panic/expect/unwrap. **Findings: M1.**
- `src/app/dispatch.rs` (330 lines) — grep for panic/unwrap/expect → 0 hits. Clean.
- `src/app/overlay.rs` (1195 lines) — Overlay enum read (lines 1-90), handle_key dispatch structure verified. Single-source key-dispatch fn, structurally sound.

### Tier-3 (grep)

- `src/vterm.rs` (718 lines) — grep panic/unwrap/expect → 0 hits in production paths. Post-PR #194 hotfix verified by inline read.
- `src/app/session.rs` (395 lines) — read trust-boundary section (lines 140-340). Acceptable trust model.

### Out of scope but adjacent (declared, not audited)

- `src/app/mod.rs` (1025 lines), `src/app/tui_events.rs` (697), `src/app/mouse.rs` (625), `src/app/pane_factory.rs` (526), `src/app/commands.rs` (324), `src/app/api_server.rs` (130), `src/app/telegram_hooks.rs` (81). Total ~3400 lines unaudited. Recommended sub-track: app/ event flow audit.

---

## Refactor opportunities

### R1 — Extract `clamp_overlay_size` helper (closes H1 + L2)

```rust
// In src/render.rs (or a new helpers module):
fn overlay_dims(area: Rect, base_height: u16, base_width: u16, h_pad: u16, w_pad: u16) -> (u16, u16, u16, u16) {
    let h = base_height.saturating_add(4).min(area.height.saturating_sub(h_pad));
    let w = base_width.min(area.width.saturating_sub(w_pad));
    let x = area.width.saturating_sub(w) / 2;
    let y = area.height.saturating_sub(h) / 2;
    (x, y, w, h)
}
```
Replace 5 unbounded subtraction sites + the existing render_menu saturating block with this single helper. ~30-line PR. Eliminates H1 + L2 simultaneously.

### R2 — Non-empty Tab.root wrapper type (closes M1)

Two refactor paths:
- **Cheap**: `Tab.root: PaneNode` (no Option). Use `mem::replace(&mut self.root, PaneNode::sentinel())` for take/replace pattern with a sentinel that callers must consume immediately. ~50-line PR + audit of all 7 expect sites.
- **Pure**: introduce a `NonEmpty<T>` newtype with `take_then_replace<F>(&mut self, f: F)` that takes `T`, calls `f(T) -> T`, and replaces. No `Option`, no expect. Cleaner abstraction, larger PR.

Both eliminate brittle invariant.

### R3 — Capital 'S' keybind (closes M2 + backlog t-20260426122506967277-9)

One-word fix: `KeyCode::Char('S') | KeyCode::Char('s') => Action::ShowStatus`. Already filed as backlog. No audit blocker; mention so coverage report is honest.

---

## Cross-area dependencies

(Per dispatch criterion 4 dual-label format: `reported_from: Track C` + `primary_owner: <area>` for cross-area issues)

### Track C → Track A (channel)

- `src/app/telegram_hooks.rs` (81 lines, not deep-audited) bridges TUI key events to Telegram channel. **Cross-area finding** (informational): TUI invokes channel directly via this bridge. *reported_from: Track C* / *primary_owner: Track A*. No issue identified — flagged for Track A's audit awareness.

### Track C → Track B (daemon + lifecycle)

- `src/render.rs` reads `AgentRegistry` (Track B's state structure) via `lock_registry` (line 42) for state coloring + `highest_priority_state` per tab. Read-only access through documented lock pattern. **No cross-area finding**; safe coupling.
- `src/state.rs` consumes `Backend` enum (Track B) in `for_backend()`. Pattern-matching dependency only. **No issue.**

### Track C → Track D (MCP handlers)

- `src/app/api_server.rs` (130 lines) bridges TUI commands to MCP handlers. Not deep-audited; **flagged for cross-pass with Track D reviewer**. *reported_from: Track C* / *primary_owner: Track D*.

### Track C → Track C internal

- `src/render.rs` ↔ `src/layout.rs`: render reads layout's pane tree for placement; layout's `find_pane`/`pane_ids` API is read-only. Boundary clean.
- `src/app/overlay.rs` ↔ `src/render.rs`: overlay state stored in `Overlay` enum, render dispatches on enum variant. One-way data flow. Boundary clean.

---

## Sprint 21 actionable tasks

(Trailing per dispatch criterion 8.)

1. **[HIGH, ~30-line PR]** Apply R1 — extract `overlay_dims` helper, fix 5 unbounded subtraction sites in render.rs + propagate to render_menu. Closes H1 + L2.
2. **[MEDIUM, ~50-line PR]** Apply R2 (cheap path) — `Tab.root: PaneNode` direct + sentinel-replace pattern. Closes M1.
3. **[trivial, 1-line PR]** Capital S keybind cover — already filed as `t-20260426122506967277-9`. Move to Sprint 21 if not picked up earlier.
4. **[MEDIUM sub-track]** Extend audit to unscanned app/ files (`mod.rs`, `tui_events.rs`, `mouse.rs`, `pane_factory.rs`, `commands.rs`, `api_server.rs`, `telegram_hooks.rs`) — ~3400 lines total. First-sweep audit may have missed app/-internal patterns.

---

## Peer-pass critique (post-Track D MCP report)

**To be appended after reading `docs/codebase-review-2026-04-27/MCP.md`.** Per dispatch criterion 10. ~1 paragraph blindspot critique focused on cross-area visibility (TUI → MCP via api_server bridge).
