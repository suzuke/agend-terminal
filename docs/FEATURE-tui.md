# TUI — Multi-Agent Management Interface

AgEnD Terminal provides a tmux-like terminal multiplexer that lets you manage, monitor, and interact with your entire agent fleet from a single screen.

## Launch

```bash
# Launch TUI (owned mode — auto-starts daemon and all fleet agents)
agend-terminal app

# Attach to an already-running daemon (attached mode — no new agents spawned)
agend-terminal app --attach
```

### Owned vs Attached Mode

- **Owned mode**: The TUI owns the daemon lifecycle. On launch it reads `fleet.yaml`, spawns all defined agents, and stops them on exit. Best for local development.
- **Attached mode**: Connects to an already-running daemon. The TUI only displays — it does not manage agent lifecycles. Best for remote connections or sharing a daemon among multiple operators.

---

## Core Concepts

### Tabs and Panes

Each tab contains one or more panes; each pane corresponds to an agent's terminal output. Panes can be split horizontally or vertically, forming a tree layout.

### Prefix Key

All operations are triggered through the `Ctrl+B` prefix (same as tmux). Press `Ctrl+B` to enter command mode, then press the corresponding key to execute.

Some operations support rapid repeat: while resizing panes with arrow keys, you don't need to re-press `Ctrl+B` within a 1.5-second window.

To send a literal `Ctrl+B` to the agent, press `Ctrl+B Ctrl+B`.

---

## Keyboard Shortcuts

### Tab Management

| Shortcut | Action |
|----------|--------|
| `Ctrl+B c` | New tab (opens selection menu for agent/backend/shell) |
| `Ctrl+B n` | Next tab |
| `Ctrl+B p` | Previous tab |
| `Ctrl+B l` | Switch to last-used tab |
| `Ctrl+B 0-9` | Jump to tab N |
| `Ctrl+B &` | Close tab (with confirmation) |
| `Ctrl+B ,` | Rename tab |
| `Ctrl+B w` | List all tabs (searchable) |

### Pane Management

| Shortcut | Action |
|----------|--------|
| `Ctrl+B "` | Horizontal split (top/bottom) |
| `Ctrl+B %` | Vertical split (left/right) |
| `Ctrl+B o` | Cycle pane focus (repeatable) |
| `Ctrl+B Arrow` | Directional pane focus switch |
| `Ctrl+B Alt+Arrow` | Resize pane |
| `Ctrl+B H/J/K/L` | Resize pane (vim-style, with Shift) |
| `Ctrl+B x` | Close pane (confirmation when multiple panes) |
| `Ctrl+B z` | Toggle zoom (fill tab with single pane; press again to restore) |
| `Ctrl+B Space` | Cycle default layout mode |
| `Ctrl+B .` | Rename pane |
| `Ctrl+B !` | Move pane to another tab |
| `Ctrl+B @` | Flip split direction (horizontal <-> vertical) |

### Scroll Mode

| Shortcut | Action |
|----------|--------|
| `Ctrl+B [` | Enter keyboard scroll mode |
| `j` / `k` | Scroll down / up |
| `Up` / `Down` | Scroll up / down |
| `PgUp` / `PgDn` | Scroll 10 lines |
| `q` / `Esc` | Exit scroll mode |

### Panels and Overlays

| Shortcut | Action |
|----------|--------|
| `Ctrl+B D` | Open decisions panel (read-only, scrollable) |
| `Ctrl+B T` | Open task board (four-column kanban view) |
| `Ctrl+B ?` | Open keyboard shortcut help |
| `Ctrl+B ~` | Open floating shell (Esc to close and terminate) |
| `Ctrl+B :` | Open command palette |
| `Ctrl+B d` | Detach (exit the TUI) |

### Mouse Controls

| Action | Effect |
|--------|--------|
| Click pane area | Switch focus |
| Click tab label | Switch tab |
| Click `[+]` button | Open new-tab menu |
| Drag tab label | Reorder tabs |
| Drag split border | Resize panes live |
| Drag pane title bar | Swap pane positions |
| Drag pane title -> tab bar | Move pane across tabs |
| Scroll wheel | Scroll focused pane (3 lines per tick) |
| `Shift+Drag` | Text selection |

---

## Command Palette

Press `Ctrl+B :` to open the command palette. Type a command and press Enter to execute.

| Command | Args | Description |
|---------|------|-------------|
| `:spawn` | `<name> [backend]` | New tab with agent |
| `:vsplit` | `<name> [backend]` | Vertical split with agent |
| `:hsplit` | `<name> [backend]` | Horizontal split with agent |
| `:layout` | `[name]` | Set layout (cycles if no argument) |
| `:kill` | `<name>` | Terminate agent and remove from fleet |
| `:restart` | `[name]` | Restart agent (defaults to focused pane) |
| `:send` | `<to> <msg>` | Send a message to an agent |
| `:broadcast` | `<msg>` | Broadcast to all agents |
| `:status` | — | Log agent status (for debugging) |

`backend` defaults to `claude`. Supported backends: claude, codex, gemini, opencode, kiro.

---

## Task Board

Press `Ctrl+B T` to open the task board. The board offers four views, toggled with `Tab`:

- **Tasks**: Four-column kanban (Backlog / Open / InProgress / Done)
- **Fleet**: Agent list with status
- **Status**: Agent health dashboard
- **Monitor**: Real-time monitoring

### Task Board Controls

| Shortcut | Action |
|----------|--------|
| `Left` / `Right` (or `h` / `l`) | Switch columns |
| `Up` / `Down` (or `j` / `k`) | Move within a column |
| `Enter` | View task details |
| `n` | Create new task |
| `d` | Cancel task |
| `D` (Shift+d) | Mark task as done |
| `a` | Assign task to an agent |
| `H` (Shift+h) | Move status left (demote) |
| `L` (Shift+l) | Move status right (promote) |
| `?` | Show board help |
| `q` / `Esc` | Close board |

---

## Session Persistence

The TUI automatically saves the current layout to `~/.agend-terminal/session.json`, including:

- Tab names and order
- Pane split tree structure and proportions
- Currently active tab

### Restoration Logic

On next launch, the TUI reconciles the session with the agent source (fleet.yaml or daemon registry):

1. **Rule 1**: Auto-launch all agents defined in fleet.yaml
2. **Rule 2**: Agents in session but not in fleet are silently removed; sibling panes reclaim the space
3. **Rule 3**: Agents in fleet but not in session are appended as new tabs, grouped by team
4. **Rule 4**: If no agents exist, create a fallback shell

This ensures fleet.yaml is always the authoritative source for the agent set, while session.json only remembers layout preferences.

---

## Terminal Compatibility

### Keyboard Protocol

The TUI supports the Kitty keyboard protocol (disambiguated escape codes). Terminals that don't support it automatically fall back to standard ANSI mode. Shift+letter keys work correctly in both modes.

### Newline Input

- `Shift+Enter`: Non-submitting newline (requires a modern terminal)
- `Ctrl+J`: Non-submitting newline (works on all terminals)

### Panic Recovery

If the TUI crashes unexpectedly, the panic hook automatically restores terminal state:

1. Disables Kitty keyboard enhancement
2. Restores ratatui terminal settings
3. Disables mouse capture
4. Shows the cursor

This ensures the terminal remains usable after a crash.

---

## Common Recipes

### Quick-Start a Three-Agent Team

```bash
# Define in fleet.yaml
instances:
  lead:
    backend: claude
    role: orchestrator
  dev:
    backend: claude
  reviewer:
    backend: claude

# Launch the TUI
agend-terminal app
```

The TUI automatically creates a tab for each agent, grouped by team.

### Add an Agent On-the-Fly

In the TUI, press `Ctrl+B c` and select a backend from the menu, or press `Ctrl+B :` and type:

```
:spawn helper claude
```

### Side-by-Side View of Two Agents

```
Ctrl+B %    # Vertical split, then select the second agent
```

Or via the command palette:

```
:vsplit reviewer
```

### Move a Pane Across Tabs

```
Ctrl+B !    # Opens the move menu — select the target tab or create a new one
```
