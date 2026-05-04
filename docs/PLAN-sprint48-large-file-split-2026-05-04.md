# Sprint 48 PLAN — Large File Split Refactor

**Date**: 2026-05-04
**Author**: lead
**Status**: PLAN (awaiting §8 GO + scope ruling)
**Source-of-truth**: `origin/main` HEAD `d7c6590` (Sprint 46 P3 just merged)
**Synthesis inputs**:
- dev STRUCTURAL — m-20260504045212393275-390
- reviewer PRIOR-ART — m-20260504045123183748-389
- reviewer COST-BENEFIT — m-20260504045313191909-392
- lead MINIMAL-DELTA — this document §5

---

## §0 Context

3 oversized files in `src/`:
- `src/render.rs` 2386 LOC
- `src/layout.rs` 2170 LOC
- `src/channel/telegram.rs` 4201 LOC
- Total ~8757 LOC

All 3 exceed common review-cognitive thresholds and the codebase's existing 700 LOC invariant for handler files (`tests/file_size_invariant.rs`). Sprint 48 splits them into sub-modules to:
- Reduce single-file LOC under ≤700 each
- Separate module responsibilities
- Preserve zero behavior change (pure refactor, §3.5.10 production-path-coupled)

Operator dispatch m-20260504044843966185-385 set Sprint 48 = large file split, original hint 3-4 PRs, ETA 3-5 days IMPL.

## §1 Goal

Reorganize the 3 large files into dir-style sub-modules with each sub-file ≤700 LOC. **Zero behavior change** invariant — all tests pass before and after, plus golden/snapshot for high-risk paths (telegram inbound message flow).

**Non-goals**:
- Logic changes inside any function
- Public API changes (use `pub use` re-exports through `mod.rs`)
- Style migration (`mod.rs` ↔ `module.rs+module/`) — defer per reviewer COST-BENEFIT m-392 §7
- Test file restructure beyond what's needed to fit ≤700 LOC

## §2 Verified state

| File | LOC | #[test] count | Test LOC | Sub-cluster count (per dev m-390) |
|------|-----|---------------|----------|-----------------------------------|
| `src/render.rs` | 2386 | 23 | ~511 | 5 (core / border / overlay / panels / scratch) |
| `src/layout.rs` | 2170 | 37 | ~743 | 6 (pane / tree / preset / split / tab / Layout struct) |
| `src/channel/telegram.rs` | 4201 | 75 | ~1770 | 11 → 2 PRs (3a inbound + 3b adapter) |

Cross-dependency observed (dev m-390 §2): `render.rs` calls `split_chunks` defined in `layout.rs`; `layout.rs` doesn't import from `render.rs` directly but depends on render via `split_chunks` callsite. **Resolution**: move `split_chunks` from `render.rs` to `layout/split.rs` since it is pure geometry.

## §3 Design — sub-module split per file

### §3.1 `src/render.rs` → `src/render/{}.rs`

| Sub-file | LOC est | Content |
|----------|---------|---------|
| `core.rs` | ~530 | render(), tab_bar, status_bar, pane_tree |
| `border.rs` | ~200 | split_chunks (REMOVED — moved to layout/split.rs), BorderCell, border_char |
| `overlay.rs` | ~400 | menu / rename / help / palette renderers |
| `panels.rs` | ~630 | decisions / tasks / fleet_view / monitor_view |
| `scratch.rs` | ~60 | scratch_shell_rect, render_scratch_shell |
| `mod.rs` | ~30 | `pub use sub::*` re-exports |

Tests (23 / ~511 LOC) distributed inline to each sub-module per locality.

### §3.2 `src/layout.rs` → `src/layout/{}.rs`

| Sub-file | LOC est | Content |
|----------|---------|---------|
| `pane.rs` | ~140 | Pane, PaneSource, Selection types |
| `tree.rs` | ~430 | PaneNode, SplitDir, tree transforms, swap |
| `preset.rs` | ~120 | LayoutPreset, build_tree |
| `split.rs` | ~250 | ratio_to_size, split_child_areas, hit-testing, **split_chunks (moved from render.rs)** |
| `tab.rs` | ~330 | DragTabTarget, Tab struct + methods |
| `mod.rs` | ~260 | Layout struct, resize_panes, TAB_BAR_HEIGHT, re-exports |

Tests (37 / ~743 LOC) distributed.

### §3.3 `src/channel/telegram.rs` → `src/channel/telegram/{}.rs` (2 PRs)

**PR 3a — core transport + inbound** (~1430 prod + ~900 test LOC):
| Sub-file | LOC est |
|----------|---------|
| `state.rs` | ~250 (TelegramState + runtime) |
| `topic_registry.rs` | ~200 |
| `send.rs` | ~200 (send primitives) |
| `inbound.rs` | ~600 (polling + handle_message) |
| `error.rs` | ~180 |

**PR 3b — adapter + outbound** (~1570 prod + ~870 test LOC):
| Sub-file | LOC est |
|----------|---------|
| `creds.rs` | ~70 |
| `reply.rs` | ~300 (reply/provenance helpers) |
| `bot_api.rs` | ~200 |
| `notify.rs` | ~100 |
| `adapter.rs` | ~650 (TelegramChannel + Channel/UxEventSink impls) |
| `bootstrap.rs` | ~220 |
| `mod.rs` | ~50 |

Tests (75 / ~1770 LOC): hybrid — small inline per sub-module + shared `tests.rs` sibling for cross-cutting fixtures (per reviewer COST-BENEFIT m-392 §4).

## §4 Phase split — 3 candidate options

### Option A — reviewer minimal viable (3 PRs in Sprint 48)
- PR 1: `layout-1` — extract `split_chunks` + new `layout/split.rs` (preflight for telegram coupling) — Tier-1 single
- PR 2: `telegram-3a` — core transport + inbound — **Tier-2 dual** (high-risk message flow)
- PR 3: `telegram-3b` — adapter + outbound — Tier-1 single

**Defer to Sprint 49**: full `render` split + remaining `layout` sub-modules (pane / tree / preset / tab).

**Pros**: 80/20 highest-risk file done in Sprint 48; reviewer bandwidth not blown.
**Cons**: render.rs still 2386 LOC at end of Sprint 48; layout.rs only partially split.

### Option B — operator original (3-4 PRs covering all 3 files)
- PR 1: `render` split (full)
- PR 2: `layout` split (full)
- PR 3: `telegram-3a`
- PR 4: `telegram-3b`

**Pros**: All 3 files done in one sprint.
**Cons**: PR 1 + 2 each move 2000+ LOC — exceeds reviewer cognitive threshold per PRIOR-ART m-389.

### Option C — reviewer full-split (6 PRs)
- PR 1: `layout-1` (split_chunks + tree/split/preset)
- PR 2: `layout-2` (pane/tab/Layout struct)
- PR 3: `render-1` (border + scratch)
- PR 4: `render-2` (core + overlay + panels)
- PR 5: `telegram-3a`
- PR 6: `telegram-3b`

**Pros**: Each PR ~800-1400 LOC moved (manageable review size).
**Cons**: 6 review cycles in one sprint = high coordination cost; wall-clock 5+ days.

## §5 MINIMAL-DELTA verification (lead vantage)

**Cross-dep cycle**: `render.rs::split_chunks` ← `layout.rs::*` callsites. Moving `split_chunks` to `layout/split.rs` per dev m-390 breaks the cycle. This must be PR 1 (or first commit of any first PR).

**Import path stability via `mod.rs` re-export**:
```rust
// src/render/mod.rs
pub use core::*;
pub use border::*;
pub use overlay::*;
pub use panels::*;
pub use scratch::*;
```

External callers see `crate::render::X` unchanged. Same for layout, telegram. **Zero caller import churn** (per dev m-390 §4).

**Behavior preservation**:
- Floor: `cargo fmt --check` + `clippy -D warnings` + full `cargo test` (no --bin filter) per accumulated lessons (test parallel + serial both)
- High-risk: `telegram-3a` inbound message handling — capture golden/snapshot of message header parsing + event dispatch sequence on origin/main, replay against PR head, compare byte-for-byte
- Low-risk: render/layout/telegram-3b — compile + tests sufficient

**Smaller alternative considered + rejected**: pull only `split_chunks` to `layout/split.rs` and call it Sprint 48. Rejected — operator's stated goal was "降單檔 LOC" across 3 files, and just the cross-dep extraction does not move any single file under 700 LOC.

**Larger alternative considered + rejected**: bundle all 3 files into one mega-PR. Rejected per PRIOR-ART m-389 §4 (incremental over mega-move).

## §6 Backward compat

- All `pub use` re-exports preserve public path stability — `crate::render::render()`, `crate::layout::Pane`, `crate::channel::telegram::TelegramChannel` unchanged
- Inline tests within each sub-module — test discovery unchanged via `cargo test`
- `#[cfg(test)] mod tests` blocks distributed by responsibility — caller test invocation paths preserved through re-export
- `git log --follow` works on the **first** sub-module that takes the original file's `git mv` (per reviewer COST-BENEFIT m-392 §6); other split-out files start clean blame

## §7 Risks

**HIGH (telegram-3a only)**:
- Inbound message flow + concurrency surface = risk of race regression. **Mitigation**: golden/snapshot test capturing 5-10 representative message types' parse+dispatch sequence, replay against PR head, byte-compare. **Tier-2 dual** review.

**MED**:
- Visibility leak (`pub(super)` → `pub(crate)` accidental promotion to fix compile errors). **Mitigation**: PR review checklist explicit — "no `pub` widened beyond what was already accessible from outside the file".
- Test reorganization breaks `cargo test --bin` filters used in CI step. **Mitigation**: verify `tests/file_size_invariant.rs` still passes (each sub-file ≤700 LOC) and `cargo test --bin agend-terminal` still discovers all moved tests.

**LOW**:
- `git blame` history reset for split-out sub-files. Acceptable per reviewer COST-BENEFIT m-392 §6 (split is structurally not a single rename).
- `mod.rs` vs `module.rs+module/` style choice — defer to operator §13.

## §8 §13 candidate questions for operator

1. **Phase split option**: A (3 PRs minimal viable, defer render+layout-2 to Sprint 49) vs B (3-4 PRs all 3 files this sprint) vs C (6 PRs all-split this sprint)?
2. **Sprint 49 plan**: if option A, when ship Sprint 49 (operator m-330 said Sprint 49 = TUI run_app extract — does render/layout-2 split fit?
3. **Tier classification**: layout-1 + telegram-3b + render Tier-1 single, telegram-3a Tier-2 dual — agree?
4. **Behavior preservation for telegram-3a**: golden/snapshot before+after refactor required? Or rely on existing 75 unit tests?
5. **`mod.rs` vs `module.rs+module/`**: maintain `mod.rs` consistency with existing repo (`src/channel/mod.rs`, etc) per reviewer COST-BENEFIT m-392 §7? Or migrate to modern `module.rs+module/`?
6. **Test distribution policy**: render/layout = inline distribute per locality. Telegram 75 tests / 1770 LOC = inline + sibling `tests.rs` hybrid. Agree?
7. **PR ordering**: layout-1 → telegram-3a → telegram-3b → (render if Option B) — confirm sequential, no parallel?
8. **`git mv` strategy**: each PR's first commit = `git mv old.rs new/core.rs` to preserve `--follow` for that sub-file. Other splits = fresh blame. Acceptable trade-off?
9. **Cross-PR dependency**: layout-1 must merge BEFORE telegram-3a/3b? (Telegram doesn't depend on layout, so technically no — just coupling cleanup ordering. Confirm parallel after layout-1.)
10. **Sprint 48 closure metric**: all 3 PRs (Option A) merged + tests pass + zero behavior regression observed. Or different threshold?

## §9 Estimates

- PR 1 (layout-1, ~250 LOC moved): ~1-2h IMPL + 1 review cycle = ~3-5h elapsed
- PR 2 (telegram-3a, ~1430 prod + 900 test LOC moved): ~6-10h IMPL + Tier-2 dual review = ~12-20h elapsed
- PR 3 (telegram-3b, ~1570 prod + 870 test LOC moved): ~5-8h IMPL + 1 review cycle = ~6-12h elapsed
- Total Sprint 48 (Option A): ~20-37h elapsed across 3-5 wall-clock days
- Option B/C add render + layout-2 = +10-20h additional

## §10 Reuse from prior synthesis

- ci.yml + Sprint 47 P1 timeout + concurrency now ACTIVE — refactor PRs benefit from CI hardening
- `feedback_test_parallel_race_check.md` lessons apply: cargo test parallel + serial both for refactor PRs
- Spawn rationale §10.5: refactor must not lose existing `// fire-and-forget:` comments during file moves

---

**End of PLAN — awaiting operator §13 answers + §8 GO**
