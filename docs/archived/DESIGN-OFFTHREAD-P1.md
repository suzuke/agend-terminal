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
- [x] S3 — WIRING (DONE, this commit):
  1. ✅ `src/layout/pane.rs`: added field `pub offthread: Option<crate::render::offthread::OffthreadHandle>` to `Pane` (init `None` at ALL 23 construction sites — 1 prod placeholder + attach_pane + remote + 20 test ctors; compiler-verified no site missed). `drain_output` top: `if self.offthread.is_some() { return 0; }` (parser is sole rx consumer; main never drains — crossbeam is MPMC so a main drain would steal chunks).
  2. ✅ `src/app/pane_factory.rs` `apply_attachment` (after forwarder spawn): flag-on → `spawn_offthread_parser(pane_id, name, pane.rx.clone(), VTerm::new(pane.vterm.cols(), pane.vterm.rows()), wakeup_tx.clone())`; `pane.offthread = Some(h)`. Dump already enqueued to fwd_tx→pane.rx above (unbounded buffer), so the parser's `pane.rx.clone()` receives dump+live — NO dump change.
  3. ✅ `src/render/core_render.rs` `render_pane`: `let cursor = if let Some(handle) = &pane.offthread { let snap = handle.load(); resize→handle.request_resize + pane.resize_pty; snap.render_to_buffer(...); snap.cursor } else { <existing vterm path unchanged> pane.vterm.cursor_pos() };` — selection block shared/unchanged; focused-cursor block uses captured `cursor`. NLL allows the else-branch `&mut pane.vterm` (scrutinee borrow not held in else).
  4. ✅ `drain_all_panes`/`drain_all_panes_until`: no change — `drain_output` no-ops (step 1) returns 0; `rx.is_empty()/len()` readers reflect the shared channel (parser drains it) and redraw is driven by the parser's `wakeup_tx` after each publish, so the `more`/re-arm accounting degrades gracefully (audited all non-consuming pane.rx readers: core_render:78/83/206 is_empty + drain_all_panes:142 len — all fine).

  **S3 SCOPE BOUNDARY (audited entry points, documented decision):** offthread parser is spawned ONLY in `apply_attachment` — which covers the freeze HOT path (synchronous `create_pane`:243 + the deferred-restore worker apply:876, i.e. the 16-agent restart storm). NOT wired: `attach_pane` (tui_events ×3 — a SINGLE API-created agent appearing live, not the bulk restart flood; also seeds dump via `vterm.process` not rx, so wiring it would need a different dump path — more invasive, out of P1 scope) and `create_remote_pane` (bridge panes). Those keep `offthread=None` → existing main-thread drain (flag-off-equivalent, zero regression). When the flag is ON those two paths are simply not off-thread-parsed — a documented P1 boundary, acceptable because they are not the freeze hot path. P2/P3 can extend coverage.
- [x] S4 — TESTS + verify (DONE, this commit):
  - `layout::pane::tests::drain_output_is_noop_when_offthread_owns_parsing` — the freeze-fix invariant: with `offthread=Some`, `drain_output` returns 0 and consumes ZERO rx chunks (main per-frame parse is bounded to nothing regardless of backlog = boot-race safe; also proves no MPMC chunk-stealing from the parser).
  - `render::core_render::tests::render_paints_offthread_snapshot_not_main_vterm` — `render_pane` paints the parser's published snapshot (content reaches the frame buffer with the main `vterm` left blank) = the render wiring works end-to-end.
  - S2 already covers off-thread-parse→publish, resize routing+dedup, and clean thread reap (parser-falls-behind/spawn-reap deterministic proxies). boot-race "N-pane" Layout-level test deemed redundant — drain_all_panes is a trivial per-pane sum and the per-pane no-op is pinned above.
  - clippy gap fixed: `offthread.rs` tests mod was missing `#[allow(clippy::unwrap_used)]` (S2 was committed without `clippy --all-targets`); added (matches core_render convention).
  - **CI parity ALL green** (CI-equivalent: real git on PATH, no global AGEND_GIT_BYPASS): `cargo fmt --check`; `cargo clippy --features tray --all-targets -D warnings`; full `cargo nextest --features tray` = 4720 tests pass (incl. spawn_rationale_audit / file_size_invariant / git_subprocess_invariant). Two transient FAILs were test-harness env artifacts, both confirmed passing clean: e2e binding test under the fleet `git` shim (pass with /usr/bin git) + `..._no_bypass_1899` when AGEND_GIT_BYPASS=1 is set globally (pass without it).
  - instrument_never_blocks: the `#offthread-snapshot` publish is a `()` fn (no return-value dependency) ✓; spawn_rationale: S3 added NO new spawn — the parser spawn lives in offthread.rs (S2) with a fire-and-forget comment ✓.
  - PR #2404 (head ea5014b1). DUAL: r4 VERIFIED core (no double-consume / ArcSwap no-tearing / no core.lock / flag-OFF byte-identity, 5 probes 38/38); r6 cross-model REJECTED 2 blocking + 1 non-blocking → S5 rework.
- [x] S5 — r6 REWORK (DONE):
  - **① pane-close thread leak (blocking)** — the parser held a clone of `pane.rx` and the forwarder held the matching `fwd_tx`, so closing a pane while its agent was alive left BOTH threads alive forever (forwarder sends keep succeeding → parser receiver keeps the channel open) → ghost accumulation on re-attach. The S2 clean-exit test was a false-green (it dropped the synthetic `data_tx`, breaking the very cycle under test). **Fix:** `OffthreadHandle` now holds a `cancel_tx` + the parser `JoinHandle`; `impl Drop` sends cancel + joins → the parser exits even with the data channel open, and dropping its `rx` clone lets the forwarder revert to its pre-existing flag-OFF lifecycle (exits on next send). **RAII Drop chosen over signalling at tab.rs/tree.rs teardown sites** (lead-approved): covers EVERY pane-drop path (close/shutdown/re-attach replace), zero missed-entry-point risk; `Pane` owns the handle so Pane-drop IS the pane-owned cancellation r6 asked for. Join is bounded (cancel checked in BOTH select phases → ≤ one chunk; parser owns per-pane VTerm/channel, zero shared lock = no deadlock; wakeup channel unbounded so the cancel-path return never blocks). Tests: `parser_exits_on_handle_drop_even_with_data_channel_open` + `close_then_reattach_does_not_accumulate_ghost_parsers` (agent alive across 3 cycles, each reaped).
  - **② data/resize ordering race (blocking)** — `select!` over separate data/resize channels is non-deterministic, so a queued resize could be applied before already-enqueued old-dims bytes → wrong wrapping. **Fix:** `drain_pending_data` flushes all queued data at the current dims before EVERY `vterm.resize`, restoring the main-thread "drain-then-resize" order. Test `resize_does_not_reorder_ahead_of_queued_data` (width-sensitive `\x1b[K`: 20 X + erase-line → empty at cols=20; if the resize jumped ahead the X's would wrap to 4 rows and survive). **Deeper edge (lead-raised, judged (a)+(c), lead-approved):** the forwarder feeds the channel async, so bytes the agent already produced but the forwarder hasn't transferred yet aren't drained, and there is a symmetric ε-skew (render thread issues `resize_pty`/SIGWINCH, parser applies `vterm.resize`). This window is NOT a regression — the MAIN-THREAD path has the same async-forwarder window (next frame parses late bytes at the then-current `pane.vterm` dims) — and (b) in-band single-channel would NOT close it either (late bytes still arrive after the resize), so it adds complexity for zero extra correctness (KISS, rejected). It is a transient that self-heals: a TUI repaints its whole screen on SIGWINCH, overwriting any briefly mis-wrapped cells → no persistent corruption. Documented on `drain_pending_data`.
  - **③ parser spawn-failure fallback (non-blocking)** — `spawn_offthread_parser` now returns `Option`; on OS thread-create failure it returns `None` and `apply_attachment` leaves `offthread = None` → the pane keeps the byte-identical main-thread drain path instead of being stranded with a dead parser.
  - `parser_loop` gained `#[allow(clippy::too_many_arguments)]` (thread entry point, 8 channels/ctx — project-idiomatic, cf. attach_agent_to_pane).
  - CI parity re-run all green (real git, no global AGEND_GIT_BYPASS): fmt --check; clippy --features tray --all-targets -D warnings; full nextest --features tray.
  - (round-2 re-DUAL: r4 VERIFIED core @ 62ec8e3e; r6 REJECTED — Drop-join unbounded under flood + forwarder-reap not proven → S6.)
- [x] S6 — r6 round-2 REWORK (DONE):
  - **① Drop-join truly bounded under flood (was the real freeze risk).** r6 correctly found round-1's join was NOT bounded: `drain_pending_data` drained until Empty (never happens under a continuous producer) and the `select!` arms were unbiased (cancel not prioritized), so a resize during a flood spun forever → the render-thread Drop-join would freeze. Fix: (a) `drain_pending_data` is bounded to the `data_rx.len()` SNAPSHOT (not until-empty) + checks cancel each iteration, returning a `cancelled` bool; (b) `parser_loop` checks `cancel_rx.try_recv()` FIRST at the top of both the outer loop and the coalesce loop (crossbeam has no `select_biased!` → manual cancel-first = deterministic priority over a continuously-ready data arm); (c) the coalesce loop breaks on the deadline explicitly (a flood keeps `data_rx` ready so the `default(timeout)` arm would never fire). Key evidence test: `handle_drop_reaps_parser_within_deadline_under_continuous_flood` (continuous producer + resize → drop off-thread → assert join completes within a deadline). Plus `drain_pending_data_flushes_snapshot_and_is_bounded` + `drain_pending_data_returns_early_on_cancel`.
  - **② forwarder reap — DECISION C (lead-ratified, with pre-existing-code evidence).** `crossbeam_channel::Sender` has NO `receiver_count()` (the round-2-planned mechanism doesn't exist), and lead verified on origin/main that the forwarder + fwd channel are PRE-EXISTING (pane_factory.rs:1/:4/:181/:209) — so the quiet-agent forwarder linger is a pre-existing managed-agent behavior, NOT off-thread-introduced. Once the parser is deterministically reaped (①), the forwarder's reap condition reverts to pre-existing (all receivers dropped + next message → exit); off-thread does not worsen or extend it. So: forwarder left as-is; precise doc on `OffthreadHandle::drop`; the pre-existing quiet-agent linger is tracked as follow-up **t-20260622053855100612-41860-5**. Rejected (A) empty-chunk-probe+dirty-flag and (B) new-Pane-field as scope-creep. Production-shaped proof that an ACTIVE agent (the freeze scenario) reaps the forwarder on pane close: `active_agent_pane_close_reaps_forwarder_freeze_scenario` (real `build_deferred_direct_pane` + `apply_attach_outcome`, agent alive, drop pane → next agent byte → forwarder exits).
  - **③ tests** added as above (flood-deadline = the core boundedness evidence; production-shaped active-agent forwarder reap; deterministic drain unit tests).
  - **④ doc** narrowed: "most TUIs repaint on SIGWINCH" (not universal), strong no-persistent-corruption claim removed; forwarder-lag window kept as pre-existing / not-a-regression.
  - NEXT: report new head → lead re-arms ci-watch + dispatches re-DUAL (r4 delta + r6 re-check ①②).

## RESUME POINTER (for fresh-context continuation)
S1+S2 are committed + tested on branch feat/offthread-parse-p1 (base origin/main f8390cc2). Foundation (snapshot type + parser thread + handle) is DONE and proven. Remaining = S3 wiring (4 edits above) + S4 tests. Read this file + the Explore map facts in SESSION-HANDOFF. Key gotchas: `cargo test --bin agend-terminal <name>` (layout/render are in the BIN, `--lib` misses them); worktree git needs `AGEND_GIT_BYPASS=1`; flag-OFF path must stay byte-identical (render_to_buffer_inner untouched).
