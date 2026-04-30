# Design: Team Layout Auto-Tab Grouping

**Task:** t-20260423014403  
**Author:** at-dev-kiro (orchestrator)  
**Reviewer:** at-dev-codex (cross-backend)  
**Status:** DRAFT v2 — post cross-backend review, pending user approval  
**Date:** 2026-04-23

---

## Problem

`create_team` with existing instances (no `count`/`backend`) only writes the logical team relationship to `teams.json`. Panes remain scattered across different tabs. The `create_instance --team` batch mode groups panes but forces `<team>-N` naming.

## Root Cause (Two Coupled Bugs)

### Bug A: API emission gated to spawned-only

`handle_create_team` in `src/api/handlers/team.rs:153-160` only emits `TeamCreated` when `spawned.is_empty() == false`, and the payload contains only `spawned_names` — not the full `all_members` roster.

- `create_team {members: [existing-a, existing-b]}` → `spawned = []` → no event → TUI never knows.
- `create_team {members: [existing-a], backend: "claude", count: 1}` → event carries only `[team-1]`, silently omitting `existing-a`.

### Bug B: TUI handler drops already-displayed members

`handle_team_created` in `src/app/tui_events.rs:370-383` filters out any member already displayed in a tab. Even if Bug A is fixed, `create_team` with all-existing members would log "no running members, no tab created" and exit.

Both bugs must be fixed together.

## Recommended Approach

### Fix A: Emit `TeamCreated` with full roster

Change `src/api/handlers/team.rs` to emit `TeamCreated` unconditionally on successful team creation, with `all_members` as payload instead of `spawned_names`.

```rust
// Before:
if !spawned.is_empty() {
    n.notify(ApiEvent::TeamCreated { name, members: spawned_names });
}

// After:
if !all_members.is_empty() {
    n.notify(ApiEvent::TeamCreated { name, members: all_members });
}
```

**Event contract change:** `TeamCreated.members` becomes "full initial team roster" (spawned + existing), not "newly spawned only". Empty teams (`all_members == []`) do not emit — TUI handler would no-op anyway, and the event contract stays intentional.

### Fix B: Move already-displayed members instead of skipping

Refactor `handle_team_created` to mirror the move-first pattern already used by `handle_team_members_changed` (lines 501-564).

**Algorithm:**

1. Partition running members into `already_displayed` (have a pane via `find_agent_pane`) and `need_attach` (registered but no pane).
2. Establish team tab:
   - If `need_attach` non-empty: `attach_pane` first member as `NewTab` root.
   - Else: `move_pane_across_tabs` first `already_displayed` member into `NewTab`.
3. Remaining `need_attach`: `attach_pane` + `split_focused` into team tab.
4. Remaining `already_displayed`: `move_pane_across_tabs` with `SplitFocused` into team tab.
5. After each `move_pane_across_tabs`, rebind `tab_idx` from the returned value (handles index shift when single-pane source tab is removed).

**Shared helper consideration (codex recommendation):** Extract the "locate-or-create team tab, then move/attach remaining members" logic into a shared function used by both `handle_team_created` and `handle_team_members_changed`. This prevents the two paths from drifting. Estimated ~10 LOC overhead for the extraction but eliminates a maintenance hazard.

## Conflict Rule: No Forced Reconciliation

- Auto-grouping fires **only** at the moment of `create_team` / `update_team --add`.
- User manual drag establishes a new local truth. System does not pull panes back.
- No periodic reconciliation. No `team_tab_id` in team model.
- `update_team --remove` only detaches from the tab whose `name == team_name`. If the member was manually dragged elsewhere, the remove is a no-op on layout (logical membership still removed from `teams.json`).

**Documented invariant:** Only explicit team mutations auto-group; manual drag overrides until the next explicit team mutation.

## Known Pre-existing Issue: Duplicate Tab Names

Team logic locates the destination tab by `tab.name == team_name`. If an unrelated tab already has that name, `update_team --add/remove` may target the wrong tab. This is pre-existing and not introduced by this design, but worth noting since we now rely on name lookup more heavily. A future enhancement could use stable tab IDs, but that's out of scope here.

## Alternative Approaches (Rejected)

| Approach | Why rejected |
|---|---|
| A. Relax `create_instance --team` naming | Doesn't solve "create first, group later" |
| B. Team owns `tab_id` in model | Major refactor (~200-400 LOC), needs stable tab IDs, tight coupling |
| C. Periodic reconciliation loop | Violates conflict rule, hostile UX, complex edge cases |

## LOC Estimate (Revised)

| File | Change | LOC |
|---|---|---|
| `src/api/handlers/team.rs` | Emit `TeamCreated` with full roster | ~5 |
| `src/app/tui_events.rs` | Refactor `handle_team_created` + shared helper | ~30-40 |
| `src/api/mod.rs` | Update existing tests for new event contract | ~15-20 |
| TUI regression test (new) | create-team with already-displayed members | ~15-20 |
| **Total** | | **~65-85** |

## Interaction with Existing Code

- **PR #70/#74 (drag/move):** No conflict. Reuses `move_pane_across_tabs`.
- **`handle_team_members_changed`:** Already correct. Shared helper extraction aligns both paths.
- **API tests (`dispatch_create_team_emits_team_created` etc.):** Must update assertions — the "no event when spawned is empty" test becomes "event with existing members when team has members".
- **Positive pin test:** Assertion on `members` payload changes from `spawned_names` to `all_members`.

## Edge Cases

1. **All members already displayed:** First → `NewTab`, rest → `SplitFocused`. ✓
2. **No members displayed, all in registry:** `attach_pane` path. ✓
3. **Mixed (some displayed, some not):** Partition handles both. ✓
4. **Member already in team tab:** `has_agent` check → skip. ✓
5. **Single-pane source tab removed:** `move_pane_across_tabs` returns adjusted index. Rebind `tab_idx`. ✓
6. **Empty team (`members: []`):** No event emitted, TUI no-op. ✓
7. **Orchestrator dragged out:** Logical role unaffected. Task routing via `teams.json`. No auto-correction. ✓

## Orchestrator Not in Tab: Fallback

No automatic correction. Orchestrator is a logical role (task routing), not a layout concept. Degraded team handling (PR #83) is independent of tab layout.
