# Option X P1 — off-thread parse (design + WIP state)

Task t-20260622033720347910-41860-0. Branch `feat/offthread-parse-p1`, base origin/main @ f8390cc2 (incl #2403 cpu_us probe). DUAL review (concurrency).

**Data that justified this**: 11:27 restart cpu_us read = ~68% CPU-bound parse + ~32% preemption → off-thread parse is the right structural fix (stagger alone insufficient).

## Scope (P1 ONLY)
- Flag `AGEND_OFFTHREAD_PARSE`, **default OFF = byte-identical current behavior**.
- Shadow-measurable when ON. **Do NOT flip default. Do NOT delete #2385/#2393/#2396** (that's P3).
- Goal: prove main-thread-zero-parse + ArcSwap snapshot cheap + no tearing + correct thread lifecycle.

## Architecture (from Explore map)
- PTY → agent broadcast → subscriber `rx` → **forwarder thread** (pane_factory.rs:416) → `fwd_tx` → `fwd_rx` (= `pane.rx`).
- OFF (today): main render thread `drain_all_panes`→`pane.drain_output`→`vterm.process` (core_render.rs:124 / pane.rs:174), then `render_pane`→`vterm.render_to_buffer` (core_render.rs:651).
- Existing off-thread parse precedent: agent PTY pump `vterm.process` under core.lock (agent/mod.rs:1677) — separate AgentCore.vterm for STATE (the "double parse"). We do NOT touch that.

## Design (minimally invasive — reuse channel+dump)
When flag ON, at `apply_attachment` (pane_factory.rs ~416, after forwarder spawn):
1. Spawn a **per-pane parser thread** that:
   - owns a fresh `VTerm::new(cols,rows)` (VTerm is Send; !Sync ok — exclusive owner),
   - consumes a **clone of `pane.rx`** (the dump+live stream already flows here via fwd_tx — no dump special-casing),
   - `select!` over (rx, resize_rx); on data: `vterm.process`; coalesce all immediately-available chunks; then publish ONE immutable `GridSnapshot` via `ArcSwap` (throttled ~16ms min interval); `wakeup_tx.send(pane_id)`,
   - on resize msg: `vterm.resize` + publish,
   - exits when rx closed (pane dropped → fwd_tx send fails → forwarder exits → rx closes). fire-and-forget (reason: pane-scoped, exits on channel close).
2. `pane.offthread = Some(OffthreadHandle { snapshot: Arc<ArcSwap<GridSnapshot>>, resize_tx, last_sent_dims: Cell<(u16,u16)> })`.

Main thread, gated on `pane.offthread.is_some()`:
- `drain_output`: early-return 0 (parser thread is sole consumer of rx clone; main never recvs).
- `render_pane`: render from `snapshot.load_full()` via `render_cells_to_buffer`; resize decision vs snapshot dims → `resize_tx.send` + `resize_pty` (dedup via `last_sent_dims`).
- Pane keeps `vterm`/`rx` fields (idle) — no Option refactor.

## Types
- `GridSnapshot { cols, rows, cells: Vec<Cell>, cursor: (u16,u16) }` — visible grid (offset 0) + cursor. Send+Sync.
- `VTerm::snapshot(&self) -> GridSnapshot` (copies visible grid + cursor).
- Extract `render_cells_to_buffer(cells, cols, rows, buf, area, cursor, show_block_cursor)` from `render_to_buffer_inner` (vterm.rs 419-491) so flag on/off share ONE wide-char render path (no tearing divergence). Existing render tests guard byte-identity.
- `arc-swap` crate dependency (pure Rust, no platform risk).

## Known P1 limitations (documented, not bugs)
- **Scrollback while flag ON**: snapshot is offset-0 (live) only; scroll-back not served off-thread in P1 (render shows live). P2 follow-up. (Flag default OFF, shadow only.)
- Pane keeps an idle `VTerm` when off-thread (memory dup ~100s KB/pane). Acceptable for P1; collapses in P3 when main-thread path removed.

## Correctness invariants (DUAL review focus)
- ArcSwap immutable snapshot → no tearing (render loads a full Arc, never mid-write).
- Zero shared lock between parser thread and AgentCore (per-pane VTerm/channel/ArcSwap) → does NOT reintroduce core.lock contention (spike ④).
- Single consumer of rx clone (main no-ops) → no work-stealing split.
- Thread lifecycle: exits on rx close; resize_tx drop benign.

## Stages (commit WIP each; CI only at PR)
- S1: arc-swap dep + flag fn + GridSnapshot + VTerm::snapshot + extract render_cells_to_buffer (flag-off path byte-identical). [foundation]
- S2: offthread module (OffthreadHandle + parser thread fn) + Pane.offthread field + drain no-op.
- S3: pane_factory wiring (spawn parser thread when flag on) + core_render render-from-snapshot + resize routing.
- S4: tests (boot-race bounded frame work + snapshot consistency; parser-falls-behind input smooth; spawn/reap) + measurement instrument (#offthread-snapshot, gated on AGEND_FREEZE_INSTRUMENT).
- Pre-PR: fmt + clippy --features tray -D warnings + bin tests + invariants; report head → DUAL.

## WIP STATUS: (update as stages land)
- [x] S1 (d1d6f7bd) — GridSnapshot + VTerm::snapshot + faithful paint copy; identity test green.
- [x] S2 (this commit) — src/render/offthread.rs: flag, spawn_offthread_parser, OffthreadHandle, parser_loop (coalesce 16ms + ArcSwap publish), #offthread-snapshot instrument; 3 tests green (off-thread parse→publish, resize routing+dedup, clean exit). Proven: VTerm Send, GridSnapshot Send+Sync.
- [ ] S3 — WIRING (the remaining work, do next):
  1. `src/layout/pane.rs`: add field `pub offthread: Option<crate::render::offthread::OffthreadHandle>` to `Pane` (init `None` everywhere Pane is constructed — pane_factory build_pane_placeholder ~189 + any test ctors). In `drain_output` (top): `if self.offthread.is_some() { return 0; }` (parser thread is sole consumer; main never drains).
  2. `src/app/pane_factory.rs` `apply_attachment` (~after forwarder spawn at :416): when `crate::render::offthread::offthread_parse_enabled()` → `let h = spawn_offthread_parser(pane_id, name, pane.rx.clone(), VTerm::new(cols,rows), wakeup_tx.clone()); pane.offthread = Some(h);`. The dump already flows via fwd_tx→pane.rx (the clone the parser consumes) — NO dump change. cols/rows = the pane's current vterm dims. (Note: need pane_id, name, wakeup_tx in scope there — confirm.)
  3. `src/render/core_render.rs` `render_pane` (~644-652): `if let Some(h) = &pane.offthread { let snap = h.load(); if let Some(d)=ResizeDecision::needed(inner, snap.cols, snap.rows){ h.request_resize(d.cols,d.rows); pane.resize_pty(registry,d.cols,d.rows);} snap.render_to_buffer(frame.buffer_mut(), inner, pane.scroll_offset, !focused); } else { <existing path unchanged> }`. Also the cursor-pos read for the focused terminal cursor (core_render ~689 cursor_pos) needs an offthread branch → use snap.cursor.
  4. `drain_all_panes`/`drain_all_panes_until` (core_render): drain_output already no-ops when offthread (step 1), so no change needed — but verify the `more`/budget accounting still behaves (no-op returns 0 drained, fine).
- [ ] S4 — TESTS + verify: boot-race (N panes concurrent flood → main-thread per-frame work bounded + snapshot consistent), parser-falls-behind (input still smooth), cross-platform spawn/reap. Then fmt + clippy --features tray -D warnings + `cargo test --bin agend-terminal` (layout in BIN not lib) + invariants (instrument_never_blocks: the #offthread-snapshot publish is a `()` fn — verify; spawn_rationale: the parser thread has a fire-and-forget comment — verify). PR → report head → DUAL review.

## RESUME POINTER (for fresh-context continuation)
S1+S2 are committed + tested on branch feat/offthread-parse-p1 (base origin/main f8390cc2). Foundation (snapshot type + parser thread + handle) is DONE and proven. Remaining = S3 wiring (4 edits above) + S4 tests. Read this file + the Explore map facts in SESSION-HANDOFF. Key gotchas: `cargo test --bin agend-terminal <name>` (layout/render are in the BIN, `--lib` misses them); worktree git needs `AGEND_GIT_BYPASS=1`; flag-OFF path must stay byte-identical (render_to_buffer_inner untouched).
