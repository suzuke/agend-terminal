# RCA — Bopomofo (注音) IME cursor-focus regression

Issue #532 — operator-reported: cursor not focused on the agent pane's command line when typing Bopomofo (注音) input; reported as a regression from earlier working behavior.

Sprint 59 Wave 2 PR-2 (Path B, doc-only). Path B = no IMPL change in this PR; the doc establishes scope, bisect rigor, and Tier estimate. IMPL ships in a follow-up PR (Wave 2 PR-3 or later) once the root cause is confirmed and the fix-shape is approved.

---

## 1. Symptom confirm + cross-backend test

**Operator-reported symptom (issue #532)**:
- Typing Bopomofo input into the Claude Code agent pane → the OS-level IME pre-edit overlay does not anchor at the command-line cursor; instead it floats at the upper-left corner of the terminal window (or at the previous cursor location).
- Operator perceives this as "cursor not focused on command line".
- Reported as a regression — earlier sessions did not exhibit this.

**Confirmation gate (operator action requested before IMPL ships)**:

The IMPL fix-shape depends on whether the regression is backend-specific or backend-agnostic. The cursor-emit code path (§3 below) operates purely on `Pane`-level state (`focused`, `scroll_offset`, `vterm.cursor_pos()`) which is identical for all backends. Therefore the *prediction* is backend-agnostic, but operator confirmation across all four backends rules out backend-specific contamination (e.g. backend-emitted escape sequences that mutate vterm cursor position differently).

Operator: please reproduce in each of the four backends below and report yes/no:

| Backend     | Reproduces?       | Notes |
|-------------|-------------------|-------|
| Claude Code | (operator: y/n)   | Original report. |
| Codex       | (operator: y/n)   | |
| Kiro        | (operator: y/n)   | |
| Gemini      | (operator: y/n)   | |

If only Claude Code reproduces → backend-specific contamination (rare; investigate `src/backend/claude.rs`-equivalent escape sequence handling). If all four reproduce → render-side cursor-emit gate or environment (terminal/macOS/dependency). Strong prior on the latter per §3 / §4.

Also requested: terminal emulator + macOS version (Terminal.app / iTerm2 / Ghostty / WezTerm / Alacritty; macOS 14.x / 15.x).

---

## 2. Bisect investigation (post-Sprint 48 → Sprint 54 range)

The dispatch named Sprint 48 PR-4 (#421, render.rs split into 7 sub-modules, commit 834f30d) as the "strongest suspect" — render-layer changes are the natural prior. Bisect findings invalidate that prior.

**Method**: `git show 834f30d^:src/render.rs | grep -nE "set_cursor_position|cursor_pos|focused.*scroll"` against the post-split `src/render/core_render.rs:400-407`. Pre-split lines 485-490 contain the identical gate:

```
if focused && pane.scroll_offset == 0 {
    let (cursor_line, cursor_col) = pane.vterm.cursor_pos();
    ...
    frame.set_cursor_position(ratatui::layout::Position::new(cx, cy));
}
```

`Pane.scroll_offset: usize` was already a layout-side field before Sprint 48 PR-1 (#414, layout.rs split, 5b663f9) — split moved the type to `src/layout/pane.rs:36` without semantic change.

`src/vterm.rs::cursor_pos()` last semantic change traces to Sprint 21 (a2acf75, 2025) or earlier; no Sprint 48-and-later commit modifies it.

`git log 834f30d..fc859a3 -- 'src/render/*.rs' src/vterm.rs` returns three render-touching commits in the post-split window through current main fc859a3. Per-commit hunk inspection (`git show <sha> -- 'src/render/*.rs'`):

- **33545c8** (#567 task stall watchdog, Sprint 59 Wave 1 PR-1) — touches `src/render/panels.rs` + `src/render/panels_fleet.rs`. All three hunks (`panels.rs` @@-485+/+541, `panels_fleet.rs` @@-214) sit inside `mod tests { }` blocks adding `dispatched_at: None, eta_secs: None` to `Task` struct test-fixture literals. Mechanical test-fixture updates following a `Task` schema extension; no production render code change. Cursor-unrelated.

- **0394405** (#514 UTC→local timezone display in expanded decision view, Sprint 54 P2-6) — touches `src/render/panels.rs`. Adds a `format_local_short` helper (RFC 3339 → `MM-DD HH:MM` local-timezone string) used by `render_decisions` for the decision-list display. No cursor-emit / cursor-track / focus-handling code path touched. Cursor-unrelated.

- **2f90cb4** (#432 Anthropic server-side rate-limit auto-retry) — touches `src/render/core_render.rs`. Sole hunk: 4-line extension of the `state_color` function adding `AgentState::ServerRateLimit` to the `Color::Indexed(208)` arm alongside `ContextFull | RateLimit`. The cursor-emit gate at the post-split `core_render.rs:400` `render_pane` function is in a different function and is not touched. Cursor-unrelated.

None of the three modify cursor-emit, cursor-track, or focus-handling code paths. The byte-identical-to-pre-split claim above (`render_pane` cursor-emit gate at `core_render.rs:400-407` matches pre-split `render.rs:485-490`) therefore stands.

`Cargo.toml` history shows `crossterm = "0.28"` and `ratatui = "0.29"` pinned, with no version bump in the post-Sprint-48 window.

**Bisect conclusion**: The application-side render and IME-adjacent input paths in this repository have not changed in any way that could plausibly explain a cursor-emit regression in the post-Sprint-48 → main range. The "Sprint 48 PR-4 strongest suspect" framing in the dispatch is therefore unsupported. The regression vector is most likely *outside* the post-Sprint-48 application diff: terminal emulator, macOS version, or a transitive dependency. See §4 for the resulting root-cause categorization.

---

## 3. Suspected file touches (predicted IMPL surface)

If the IMPL fix targets the application-side cursor-emit path (most likely surface per §4), the predicted file touches are:

- `src/render/core_render.rs:400-407` — primary cursor-emit site for normal panes; gates on `focused && pane.scroll_offset == 0`.
- `src/render/scratch.rs:60` — scratch-shell cursor-emit; ungated. Reference site for "always emit when this pane has the user's typing".
- `src/render/overlay.rs:117, 380` — overlay (rename/command-palette) cursor-emit; ungated. Reference site for the same pattern.
- `src/vterm.rs::cursor_pos` (line ~280) — read-only consumer; modify only if cursor-position semantics need to differ during IME composition (low likelihood, see §4).

If the IMPL fix targets the input/event pipeline (lower likelihood per §4):

- `src/app/mod.rs:472-514` — top-level `Event::Key` dispatch; `KeyEventKind::Press` filter at line 474.
- `src/app/mod.rs:538-547` — `Event::Paste` handler (relevant if the operator's terminal commits IME output as bracketed-paste).

The IMPL PR is expected to touch one or two files in the §3 set. A multi-file refactor is out of scope (see §7).

---

## 4. Root-cause category

Mapped to the categorization the dispatch requested (cursor-emit / cursor-track / render-redraw / combination):

**Primary category — cursor-emit (most likely)**

The render-side cursor-emit gate at `src/render/core_render.rs:400` skips `frame.set_cursor_position(...)` for any frame where the focused pane has `scroll_offset != 0`. ratatui's contract is: if no `set_cursor_position` was called during `Terminal::draw`, crossterm hides the terminal cursor for that frame. Hidden cursor → OS-level IME pre-edit overlay loses its anchor and floats at the upper-left (Terminal.app default) or at the last-known cursor position (iTerm2). The operator perceives this as "cursor not focused on command line".

The gate exists for a defensible reason: when the user has scrolled the pane into history (Ctrl+B+up etc.), the visual cursor should not jump back to the live row at the bottom — it would be misleading. So the gate is correct for the scroll-history case. The IME-composition case is an unintended interaction.

The §2 bisect rules out an in-repo regression: the gate has been in place verbatim since at least Sprint 21–25 era. The "regression from earlier working behavior" the operator reports is most plausibly:

- (a) Environment-side: the operator's terminal emulator changed IME pre-edit anchor behavior in a recent update (Terminal.app, iTerm2 IME enhancements, Ghostty kitty-keyboard-protocol changes). On older terminal versions, the OS-level IME overlay may have anchored at the last-known cursor position even on hidden-cursor frames; on newer versions it floats at upper-left.
- (b) macOS version: macOS 14→15 changed system IME composition; Bopomofo (注音) input service `com.apple.inputmethod.TCIM.Zhuyin` had behavior changes around the same window.
- (c) Operator's typing pattern: if the operator used to never have `scroll_offset > 0` while typing, but now occasionally does (mouse-wheel inertia, accidental scroll), the previously-latent bug now manifests.

Hypotheses (a)/(b) cannot be falsified from inside the repo; they require operator environment information (§1 confirmation gate). Hypothesis (c) is testable: operator reports whether `Ctrl+B` then `End` (or whatever scrolls back to live) restores the cursor and unblocks IME compose.

**Subordinate categories (lower likelihood)**:

- *cursor-track*: `vterm::cursor_pos()` returning a stale or wrong position during IME compose. Falsified by §2 (no commit changed this function in the regression window).
- *render-redraw*: a redraw-frequency drop during IME compose causing cursor visibility flicker. Render loop draws on every event tick; not a known throttle path. Lower priority than cursor-emit.
- *combination*: cursor-emit + an event-pipeline interaction (e.g. `KeyEventKind::Press` filter at `src/app/mod.rs:474` dropping a frame where the cursor would otherwise have been emitted). Possible but requires evidence; investigate only if the cursor-emit fix in §5 doesn't fully resolve.

---

## 5. Fix-shape recommendation

**Tier-1, ≤200 LOC** (general self-decide threshold). Recommended IMPL approach:

Loosen the cursor-emit gate at `src/render/core_render.rs:400` so that the cursor is emitted in scenarios where the operator is plausibly typing into the pane, even when `scroll_offset != 0`. Two candidate shapes — the IMPL author should pick whichever review feedback prefers:

- **Shape A (minimal, recommended)**: Always emit cursor for the focused pane; remove the `pane.scroll_offset == 0` half of the gate. Trade-off: when the user scrolls into history, the cursor visually appears at the live row position even though that row may be off-screen — ratatui clamps the cursor to inner-rect bounds via the `cx < inner.x + inner.width && cy < inner.y + inner.height` guard at line 404, so the cursor will simply not render when the live row is scrolled past, which is the same effective behavior as today. Estimated diff: -1 line in the `if` condition. ≤5 LOC.

- **Shape B (compromise)**: Keep the scroll-history gate but add an "operator is composing IME / has typed in last N ms" exception. Track a `last_input_at: Instant` on `Pane` (set in `pane.write_input`); emit cursor when `focused && (scroll_offset == 0 || last_input_at.elapsed() < threshold)`. Trade-off: introduces a timer-based heuristic. Estimated diff: ~30-60 LOC across `src/layout/pane.rs` (field + getter) and `src/render/core_render.rs` (gate update). Reference: `pane.write_input` lives in `src/layout/pane.rs` per current layout.

Shape A is recommended because the bisect found no in-repo regression — i.e. the gate is more aggressive than it needs to be even in the non-IME case. The §2 finding that the gate has not changed since pre-Sprint-48 means Shape A is unwinding old over-restriction, not introducing new behavior.

**LOC estimate**: 5-60 LOC across one or two files. Comfortably under the 200 LOC general self-decide threshold.

**Tier-2 IMPL (>200 LOC) is not recommended** for this regression. A Tier-2 fix would imply per-pane IME-state tracking, terminal-emulator capability detection, or rewriting the cursor-emit pipeline to be IME-aware — none of which the symptom requires. If post-IMPL the regression persists, escalate to operator before expanding scope (see §6).

---

## 6. IMPL gate / conditional escalation

**Self-decide ceiling: 200 LOC**. The fix-shape recommendation in §5 falls comfortably under this ceiling. The IMPL agent (Wave 2 PR-3 or later) ships under Path A general self-decide if both:

- Total diff (IMPL + tests + doc) ≤ 200 LOC.
- §1 confirmation gate result is consistent with §4 primary category (cursor-emit). I.e. operator confirms the symptom appears across multiple backends, ruling out backend-specific contamination. If only Claude Code reproduces, escalate.

**Operator-escalate triggers** (any of these → halt IMPL, send `kind=query` to operator before proceeding):

- §1 confirmation result indicates backend-specific reproduction (e.g. only Claude Code) — root cause moves out of `src/render/core_render.rs` and into `src/backend/*` or vterm escape-sequence handling.
- Diff trends toward >200 LOC during implementation.
- Shape A (minimal) is implemented and a follow-up reviewer round identifies a regression in scroll-history cursor visibility (operator's scroll-history workflow breaks). In that case, fall back to Shape B and re-estimate.
- Operator environment data indicates the regression is purely terminal-emulator-side or macOS-side (no application fix is correct; the doc-only outcome is to record the finding and recommend the operator file upstream with the terminal vendor / Apple).

**Verification expectations for the IMPL PR** (Wave 2 PR-3 or later):

- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test` (unit + integration)
- File-size invariant (700 LOC handler ceiling)
- Tool count invariant (currently 31)
- Operator manual-verify: type Bopomofo into the agent pane on the same terminal/OS combination that produced the regression. Cursor anchors at the command line.

---

## 7. Out of scope

Explicitly out of scope for this RCA and the follow-up IMPL PR:

- Fixing IME composition for non-Bopomofo input methods (Pinyin, Cangjie, Japanese kana, Korean Hangul). The cursor-emit gate fix is input-method-agnostic, so it should also help these — but verifying it is the operator's call, not a gate on this PR.
- Building an IME-composition-aware overlay inside the application (showing pre-edit text in a ratatui-rendered widget instead of relying on the OS overlay). That is a Tier-2+ feature, not a regression fix.
- Detecting terminal emulator capabilities at runtime to tailor cursor-emit behavior per terminal. The §5 Shape A fix is environment-agnostic; runtime detection is unnecessary unless Shape A regresses some terminal.
- Re-evaluating the `KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES` flag at `src/app/mod.rs:78`. The bisect did not implicate this flag; modifying it would risk regressing key-disambiguation correctness for non-IME workflows.
- Modifying the `KeyEventKind::Press` filter at `src/app/mod.rs:474`. The filter is correct (avoids double-firing on Windows); modifying it would risk regressing Ctrl+B prefix state and other shortcuts.
- Changing the `EnableBracketedPaste` mode at `src/app/mod.rs:66` or its `Event::Paste` handler at `src/app/mod.rs:538`. The bracketed-paste path correctly routes to `pane.write_input`, which does not interact with `scroll_offset`.

---

**Summary**: Path B doc-only RCA. Bisect rules out the dispatch's Sprint 48 PR-4 prior — the cursor-emit gate predates the split verbatim. Most-likely root cause is the in-place cursor-emit gate `focused && pane.scroll_offset == 0` interacting unintentionally with OS-level IME pre-edit anchoring; environment-side trigger (terminal emulator or macOS update) explains the "regression" framing. Recommended fix is a Tier-1 ≤200 LOC patch to `src/render/core_render.rs:400` (Shape A: drop the scroll-offset half of the gate). Operator confirmation across four backends gates the IMPL PR.
