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

**Read**: dev-reviewer-2's `MCP.md` (Track D, 199 lines, audited `1485e85`). Per dispatch criterion 10, ~1 paragraph blindspot critique:

Track D's most actionable finding is C1 (`decisions::update` no author gate, path-keyword auto-Critical) and the report's framing — *"a compromised or prompt-injected agent could update_decision { id: 'd-...', archive: true }"* — is correct as far as it goes, but **stops one hop short of the full attack chain**. Track D's Cross-area dependencies table lists daemon-API, telegram, and tasks-handle linkages, but **omits `src/app/api_server.rs`** (130 lines, in Track C scope but explicitly cross-pass-flagged in my §Cross-area as primary_owner: Track D). `api_server.rs` is the TUI→MCP bridge for command-palette-driven tool calls; if a prompt-injected response can mock a command-palette input or if api_server's parse layer trusts arbitrary JSON from upstream, the C1 attack surface widens beyond "compromised agent in fleet" to "any prompt-injection that reaches a TUI input field". Track D's Coverage caveat (handlers.rs 90% NOT deep-read) compounds the same blindspot — the auth-relevant handlers Track D *did* read are correct, but TUI-side entry into MCP is the surface neither track audited line-by-line. **Recommended sub-track for Sprint 20 follow-up**: a Track-C-Track-D joint audit of `src/app/api_server.rs:1-130` + the corresponding `mcp/handlers.rs` route arms it terminates into, treating this as a single bridged surface rather than two areas with a hand-off in between. Praise for Track D's R3 sub-bucketed Praise (replicate / preserve-as-is / refactor-eventually) — that schema is the cleanest output of all 4 tracks and I'm adopting it retroactively for my §Praise sub-buckets above; Track D also caught the `task done` vs `task update --status done` semantic divergence (M3) which is exactly the kind of dual-path drift that v1.2 §10.3 lifecycle was meant to formalize but didn't lock down at the schema level.

---

## Cross-validation: Daemon (Sprint 20.5 missing-pair B↔C)

**Read**: dev-reviewer-2's `DAEMON.md` (Track B, 285 lines, audited `1485e85`). Per Sprint 20.5 Track 7 dispatch (`t-20260426225925558096-14`), TUI auditor angle on Track B's findings.

### Confirmed findings from Daemon (✅ peer-confirmed from TUI angle)

- **Daemon F1 (`spawn_agent` partial-failure phantom registry, Critical)** — confirmed and **TUI-angle sharpened**: phantom registry entry (registry inserted but `pty_read_loop` thread spawn fails) manifests in render layer as: tab bar shows the agent name (`render_tab_bar` line 96-105 reads `layout.tabs.iter()` — pane.fleet_instance_name persists), `highest_priority_state` stays at default `AgentState::Idle` because `state.rs::feed()` is never called (no PTY tokens flowing), pane area renders empty vterm grid. **Operator sees a "starting" agent that never starts** — silent UX failure with no error indicator. This is sharper than Daemon's "freeze visible to operator" framing; useful for Sprint 21 fix prioritisation.
- **Daemon F3 (`kill_agent` app-mode SIGKILL leader-only, Critical)** — confirmed and **cascades into state.rs / render**: when app-mode kill orphans kiro-cli's child tree, `state.rs::feed()` continues to classify orphan-child PTY output (bun/mcp/acp emit trailing logs / shutdown messages), pattern matchers (`StatePatterns::for_backend`) hit ToolUse/Thinking regexes against tail tokens. Render shows orange dot + "tool_use" status for an agent the operator just explicitly killed. **Operator-visible inconsistency** between intent ("I killed it") and state ("still working"). F3 fix (`kill_process_tree` + parity with API delete) closes this UX symptom too — worth flagging in S21-B3 success criteria.
- **Daemon F5 (TUI server spawn no rollback, High)** — confirmed and **TUI-angle deepened**: F5 says missing TUI socket = no attach. From render layer: `serve_agent_tui` runs in `app/api_server.rs` (start_api_server line 41+) for the in-process API server. If the **per-instance** TUI socket spawn (Daemon mod.rs:1080, 690) fails, render has no signal — the agent shows as registered, healthy, attachable, but `attach` command silently fails to bind. Render is single-sourced from `AgentRegistry`; two-sourcing (registry + TUI socket health) is the systemic gap. Sprint 21 task: render-layer "TUI bridge unavailable" badge when the per-instance socket isn't reachable.
- **Daemon "13+/0 graceful spawn systemic"** — **CONFIRMED extends to Track C scope**. Daemon's JoinHandle inventory counted 11 sites in B; my grep over Track C scope (`render.rs`, `layout.rs`, `keybinds.rs`, `state.rs`, `vterm.rs`, `app/`) finds **2 more unnamed `std::thread::spawn` sites**:
  - `src/app/telegram_hooks.rs:56` (`maybe_create_telegram_topic`) — no `Builder::new().name(...)`, JoinHandle fully discarded (no `let _`, no `.ok()` — closure result simply dropped)
  - `src/app/telegram_hooks.rs:76` (`maybe_delete_telegram_topic`) — same pattern
  Both fire-and-forget with rationale comment in module docstring ("background threads to avoid blocking the TUI event loop"), but neither named in thread dumps and neither shutdown-aware. Total fleet-wide approaches **13 spawn sites, 0 with stored JoinHandle for graceful join**. Daemon's R3 (tick registry) framing is correct but the systemic problem is broader than ticks — every `thread::spawn` site needs the same Builder-name + rationale-comment treatment. **Sprint 21 task**: append telegram_hooks.rs:56 and :76 to S21-B10's audit-and-comment sweep.

### Missed findings discovered (TUI angle from Daemon)

- **F2 race × PR #195 hotfix area session restoration** [B+C cross-area, severity Medium]: Daemon F2 describes PID re-use + concurrent spawn race in the window between registry remove and child exit. From `app/session.rs::restore_with_reconciliation` (line 138+, the PR #195 hotfix area): startup path reads fleet.yaml + session.json, then iterates per fleet entry calling `pane_factory::create_pane_from_resolved` with `SpawnMode::Resume` (line 320). If a previous daemon session crashed mid-delete (F2 partial-failure, registry mutated but process still alive), session restore on next startup hits "spawn name X but PID-from-prior-session still owns shared resources (working dir, IPC port file)" — Daemon F2 case (b). **Cross-area implication**: F2 fix (S21-B2 synchronous wait on child exit) **must** land before the session-restore code path can claim race-free behavior. **Recommendation**: add an explicit dependency note to S21-B2: "session.rs:135 `restore_with_reconciliation` is downstream of this fix — its PR #195 hotfix protected against one branch of the race (HashMap iteration order), but the F2 PID-reuse window is a separate, cross-area branch." If S21-B2 lands without session.rs awareness, the hotfix benefit is partial.
- **F4 pty_read_loop shutdown × vterm late-write** [B+C cross-area, severity Low]: Daemon F4 notes pty_read_loop blocks on `read()` syscall and only wakes on PTY EOF. From `vterm.rs::render_to_buffer` (post-PR #194 bounds-check fix lines 140-143): vterm reads `grid.screen_lines()` / `grid.columns()` from alacritty `Term` mutated by the read loop. If F4's "shutdown short-circuits before kill propagates" race fires, late tokens land in `Term` after render is committed. PR #194 fix prevents the OOB panic, but doesn't prevent **render flicker** (last frame shows stale state, next frame shows new tokens that arrived after shutdown signal). Operator sees a brief flash of post-shutdown content. Not a correctness bug, but visible UX artifact. Sprint 21 minor: vterm could observe a `shutdown: AtomicBool` and clamp grid mutation; OR explicitly accept the flicker as harmless.
- **F8 health-tracker race × render color flicker** [B+C cross-area, severity Low]: Daemon F8 says respawn momentarily restores fresh `HealthTracker` with zero crash count → `describe_instance` MCP shows "Healthy" briefly. From render: `render.rs:42-44` reads `lock_registry` then `core.lock().ok()` to get `AgentState`. AgentState includes health-derived fields (Crashed/Restarting are highest priority — see state.rs:75). During the F8 microsecond window, render's `state_color` (line 25) returns `Color::Green` for "Healthy" briefly, then flips back to "Restarting" yellow on next frame. **Operator-visible flicker**: a chronically crashing agent flashes green during respawn. Sub-frame rendering, but TUI runs at ~30fps so a single-frame flicker is humanly perceivable on slow respawns. F8 fix (S21-B7) inherently fixes this UX symptom too.

### Disagreement / scope dispute

(none significant — all 12 Daemon findings are valid from TUI angle.)

One **scope clarification**: Daemon F5's spawn site `daemon/mod.rs:1080` is correctly Track B (file location) but the spawned function `serve_agent_tui` body lives in `app/api_server.rs` (which Daemon explicitly notes is "out of scope for Track B"). My TUI audit also did not deep-read `app/api_server.rs` (130 lines, declared as Tier-3 unaudited in §Coverage). **Recommendation for Sprint 20.5 / 21**: the C+D joint sub-track I recommended in my MCP peer-pass should be **expanded to a B+C+D triangulation** covering `serve_agent_tui` end-to-end (spawn site B → server body C → tool call routing D). Three-track audit on a 130-line file is high ROI given F5's silent-failure gravity.

### Cross-area systemic patterns not in SYNTHESIS.md

- **B↔C lifecycle/render coupling**: every Critical Daemon finding (F1/F2/F3/F4) creates a registry-vs-real-process divergence window where render shows stale/inconsistent state. The render layer is single-source (reads `AgentRegistry` via lock); the underlying source-of-truth (process state, PTY socket health, child tree liveness) is not surfaced to render. Sprint 21 systemic improvement: introduce a **transient state badge** ("respawning" / "killing" / "TUI bridge unavailable") on tab bar / pane title so operator sees the registry-vs-process gap during the windows the Critical findings describe. Currently the windows are silent. This is broader than any single F1-F8 fix and worth its own scope decision.
- **Spawn-site naming + JoinHandle convention systemic** (extends Daemon's S21-B10 finding from B-only to B+C): 13 unnamed/dropped spawn sites fleet-wide. Recommendation: protocol-level rule that every `std::thread::spawn` MUST use `Builder::new().name(...)` and either store JoinHandle for graceful join OR document `// fire-and-forget: <reason>` rationale at the call site. Pattern-conformance test (similar to `handle_message_body_has_no_block_on` invariant test from telegram.rs) could enforce this — `rg "thread::spawn" --type rust` against an allowlist of explicitly-rationalised sites.
- **Session restore is downstream of every lifecycle fix** (extends Daemon's R1 lifecycle-module proposal): `app/session.rs::restore_with_reconciliation` is the cold-boot consumer of every invariant Daemon F1-F8 establishes. Any partial-state on disk from a crashed prior session feeds back into the next session's render layer. SYNTHESIS.md treats lifecycle as B-internal; from C angle, session-restore is the **single most important consumer** of B's invariants and warrants explicit cross-area treatment in any Sprint 21 lifecycle PR.
