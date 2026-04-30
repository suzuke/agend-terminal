# Vterm Panic Postmortem — 2026-04-27

## Incident Timeline

- **~15:30 UTC**: Daemon panic at `src/vterm.rs:167:33` — `index out of bounds: the len is 26 but the index is 107`
- **5 restart attempts**: All re-panic on the same line. Poison escape sequence persists in PTY scrollback across daemon restarts — alacritty replays the scrollback on init, re-triggering the panic.
- **6th restart**: Lucky — scrollback rotated past the poison sequence.
- **Prior incident**: ~2026-04-21, same vterm panic class (different surface site).

## Root Cause

**TOCTOU race** between grid dimension snapshot and grid cell access.

```
Thread A (PTY reader):     resize(26, 10) → grid shrinks to 26 cols
Thread B (TUI renderer):   cols = self.cols.min(grid.columns())  // snapshot: 26
                           ... time passes, Thread A resizes again ...
                           grid[Point::new(line, Column(107))]   // PANIC: grid is 26 cols
```

The `.min(grid_cols)` cap at render entry (L151) is a point-in-time snapshot. Between the cap and the actual grid access (L167+), the PTY reader thread can resize the grid, shrinking it below the capped value.

### Poison vector

xterm SGR mouse tracking sequences (`CSI < btn ; col ; row M`) embed column values from the terminal emulator. When the terminal is wider than the vterm grid (e.g., terminal at 120 cols, vterm at 26 cols), mouse events carry `col=107` which exceeds the grid width.

### 5 vulnerable sites

| Line | Function | Was protected? |
|------|----------|---------------|
| 167→188 | `render_to_buffer` | Partially (`.min(grid_cols)` cap, but TOCTOU) |
| 251→272 | `selected_text` | ❌ No cap at all |
| 284→305 | `tail_lines` | ❌ No cap at all |
| 344→365 | `dump_screen` (scan loop) | ❌ No cap at all |
| 361→382 | `dump_screen` (emit loop) | ❌ No cap at all |

### Persistence

PTY scrollback is maintained by alacritty_terminal across the vterm lifetime. When the daemon restarts, it creates a fresh vterm, but the agent's PTY process is still running with the same scrollback. The poison sequence in scrollback gets replayed on reconnect, re-triggering the panic.

## Fix (this PR)

### L0a: Per-access bounds check (`safe_cell` helper)

All 5 `grid[Point::new(...)]` sites replaced with `safe_cell(grid, line, col)` which checks `col < grid.columns()` before indexing. Out-of-bounds access returns a static default blank cell.

### L0b: `catch_unwind` safety net

`render_to_buffer` wrapped in `std::panic::catch_unwind`. Any panic from grid access (including future new sites) is caught and logged — daemon stays alive, renders blank frame.

### L0c: Regression tests (7 new)

- `safe_cell_oob_column_returns_default` — column > grid width
- `safe_cell_oob_line_returns_default` — line > grid height
- `dump_screen_survives_cols_exceeding_grid` — simulated resize race
- `tail_lines_survives_cols_exceeding_grid` — simulated resize race
- `render_to_buffer_catch_unwind_safety_net` — extreme mismatch
- `xterm_mouse_tracking_large_col_no_panic` — real poison sequence class

## Why PR #194 and PR #225 deferred the same root cause

### PR #194 (2026-04-21 HOTFIX)

Added `.min(grid_cols)` cap to `render_to_buffer` (L151). Explicitly deferred the TOCTOU race to backlog: "root-cause race deferred to backlog `t-20260426150432078733-1`". Fixed only the immediate panic site, left 3 other sites unprotected.

### PR #225 (2026-04-26 Q7 sweep)

Reviewer-authored doc-only sweep confirmed the `.min()` chain was correct and explicitly noted "deferred root race" in the comment. Did not escalate the deferred backlog item to P0 despite it being a known production panic.

### Process gap

The defer chain (PR #194 → PR #225 → today) reveals 3 systemic issues:

1. **No P0 trigger for known production panics**: A deferred backlog item for a production panic should auto-escalate to P0. Instead, it sat at default priority for 6 days.

2. **No SLA on deferred backlog**: `t-20260426150432078733-1` had no `due_at`. Deferred items without deadlines are effectively "never fix".

3. **No escalation veto on repeated defer**: PR #225 was the second time the same root cause was acknowledged and deferred. No protocol required dual-reviewer sign-off or operator approval for a second defer of the same issue.

## Process Improvement Proposals

### (a) Known-issue P0 trigger

When a production panic matches a deferred backlog item, the backlog item auto-escalates to P0. Implementation: `task update` with `--priority urgent` when a panic log matches a known task description.

### (b) Deferred backlog SLA

Every deferred backlog item must have `due_at` (default: 2 sprints). Items past `due_at` auto-escalate to P0. Implementation: `task create` requires `--duration` for deferred items.

### (c) Dual reviewer escalation veto

Second defer of the same root cause requires: (1) dual reviewer acknowledgment, (2) operator sign-off. A single reviewer cannot defer the same class twice. Implementation: protocol amendment to §3.5.
