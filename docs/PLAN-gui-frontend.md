# GUI Frontend — Analysis & Roadmap

> Date: 2026-04-16
> Status: Planning (not started)
> Decision: Tauri recommended, but core feature completeness is higher priority

---

## 1. Why GUI? (Benefits)

### User Experience
- Rich visual UI: agent state visualization, message flow diagrams, drag-drop pane arrangement, right-click menus, tooltips, progress bars, image/markdown preview
- Lower barrier to entry: TUI keybind learning curve is steep; GUI lets non-power-users onboard
- Multi-window support: detach agents into separate windows, multi-monitor workflows
- Better terminal rendering: xterm.js has mature ligature, IME, selection, link detection support out of the box

### Architecture
- Forces clean core extraction: splitting into a library crate makes the agent/daemon/fleet/mcp API boundary explicit — benefits MCP, CLI, Telegram frontends too
- API-first design: the existing `api.rs` JSON-RPC gets fleshed out into a complete API, enabling future integrations (VS Code extension, web dashboard, third-party tools)
- TUI coexistence: GUI is an additional frontend, not a replacement. `agend app` (TUI) continues to work

### Ecosystem & Adoption
- Broader audience: most developers won't learn a custom TUI keybind set; GUI trial barrier is much lower
- Differentiation: nearly all agent orchestrators are CLI/TUI-only; GUI is a visible selling point
- Demo-friendly: GUI is far more intuitive for presentations and onboarding

---

## 2. Risks

### Engineering Cost
- **This is a second product.** Layout engine, state management, keybind system, session persistence — everything built for TUI must be rebuilt in the new framework
- `app.rs` alone is 81K — TUI frontend complexity is high; GUI version will be equal or greater
- Two frontends to maintain: every new feature (overlay, agent state, fleet operation) must be implemented twice

### Technical Risks
- **xterm.js in Tauri WebView**: known issues with WebView2 (Windows) and WKWebView (macOS) GPU acceleration and key event handling vs Chromium. Platform-specific bugs likely
- **PTY stream latency**: Rust → serialize → WebSocket/IPC → JS → xterm.js adds several layers vs TUI's crossbeam channel → VTerm → ratatui path. Noticeable during high-throughput output (e.g., `cat` large files)
- **Tauri maturity**: v2 is relatively new, smaller ecosystem than Electron, fewer community answers for edge cases
- **WebView memory**: each Tauri window = one WebView instance. Multiple windows consume significantly more memory than TUI

### Project Risks
- **Attention split**: core features (fleet management, MCP tools, Telegram, multi-backend) are still iterating rapidly. Months on GUI means core feature freeze
- **Half-finished is worse than none**: an incomplete GUI leaves two incomplete frontends. Users don't know which to use, maintainer doesn't know which to invest in
- **Team size**: Alacritty (just a terminal emulator) requires multi-person full-time effort. Adding agent orchestration UI on top is substantial

### Counter-arguments (reasons GUI might not be needed yet)
- Target users are developers who live in terminals
- TUI advantages (SSH remote, low resource, fast startup, scriptable) matter in devtool context
- Agent orchestrator market is early — feature completeness matters more than UI polish right now

---

## 3. Architecture Options Evaluated

### Option A: Tauri (Recommended)

**Approach**: Web frontend (React/Svelte) + Rust backend via Tauri IPC. Terminal via xterm.js.

**Pros**:
- Existing Rust backend needs minimal changes — `api.rs` JSON-RPC methods map nearly 1:1 to Tauri commands
- xterm.js is the most mature web terminal solution
- Fast UI iteration with web technologies
- Cross-platform (macOS, Linux, Windows)

**Cons**:
- WebView quirks across platforms
- Extra serialization layer for PTY streams
- Tauri ecosystem still growing

**Effort estimate**: Medium-high. Core extraction ~1 week, basic scaffold ~1 week, terminal integration ~2 weeks, feature parity ~4-8 weeks.

### Option B: wgpu + Custom Rendering

**Approach**: GPU-accelerated custom rendering like Alacritty. Already using `alacritty_terminal` for VTerm.

**Pros**:
- Maximum performance, single binary, no web layer
- Full control over rendering pipeline

**Cons**:
- Must implement font rasterization (glyphon/cosmic-text), input handling, IME, clipboard
- Layout system from scratch
- 3-5x engineering effort vs Tauri
- Performance gains not meaningful for this use case (bottleneck is agent output, not rendering)

**Effort estimate**: Very high. Months of full-time work.

### Option C: egui / iced

**Approach**: Rust-native GUI framework with embedded terminal widget.

**Pros**:
- Pure Rust, no web layer
- Simpler than wgpu custom rendering

**Cons**:
- Terminal widget ecosystem immature (`egui_term` is early-stage, iced has none)
- Bulk of effort spent making terminal rendering correct — a solved problem in xterm.js
- Limited styling/theming compared to web

**Effort estimate**: High. Terminal widget alone could take weeks.

### Option D: Electron

**Approach**: Full web stack with Node.js backend bridging to Rust core via FFI/subprocess.

**Pros**:
- Most mature desktop app framework, largest ecosystem
- xterm.js works perfectly in Chromium

**Cons**:
- Two runtimes (Node.js + Rust) — architecture mismatch
- Heavy memory/disk footprint
- Doesn't leverage existing Rust codebase well

**Effort estimate**: Medium, but ongoing maintenance burden from dual runtime.

---

## 4. Recommended Implementation Plan (Tauri)

### Phase 0: Core Extraction (prerequisite)

Restructure into Cargo workspace:

```
agend-terminal/
├── crates/
│   ├── agend-core/        # agent, daemon, fleet, mcp, state, health, backend, framing
│   └── agend-tui/         # app, render, layout, vterm, keybinds (existing TUI)
├── tauri-app/             # (Phase 1+)
│   ├── src-tauri/         # Tauri backend, imports agend-core
│   └── src/               # Web frontend
├── Cargo.toml             # Workspace root
└── fleet.yaml
```

Key decisions:
- `agend-core` exposes: `AgentRegistry`, `spawn_agent()`, `subscribe_with_dump()`, fleet resolution, MCP server, daemon lifecycle
- `agend-tui` depends on `agend-core` + ratatui/crossterm/alacritty_terminal
- Existing binary (`agend-terminal`) built from `agend-tui`

**This phase has standalone value** — cleaner codebase even without GUI.

### Phase 1: Tauri Scaffold + Agent Dashboard

- Tauri project with basic window
- Agent list view: name, backend, state (colored), health
- Fleet actions: start/stop fleet, spawn new agent
- Tauri commands wrapping `api.rs` methods: `list`, `kill`, `respawn`, `shutdown`

### Phase 2: Terminal Integration

PTY stream bridging — two options:

| Approach | Mechanism | Pros | Cons |
|----------|-----------|------|------|
| **WebSocket** (recommended) | Embedded WS server in Tauri backend, xterm.js `attach()` | Industry-standard, well-tested | Extra port/server |
| **Tauri events** | `app_handle.emit("pty-output", data)` | No extra server | JSON serialization overhead on binary data |

- xterm.js pane per agent
- Input: xterm.js `onData` → Tauri command → `inject_to_agent()`
- Output: `subscribe_with_dump()` → WebSocket → xterm.js
- Resize: xterm.js `onResize` → Tauri command → PTY resize

### Phase 3: Layout & Tabs

- Resizable split panes (CSS-based or library like `allotment`)
- Tab bar with agent state indicators
- Session persistence (save/restore layout to JSON)
- Keyboard shortcuts (subset of TUI keybinds)

### Phase 4: Advanced Features

- Agent state timeline visualization
- Inter-agent message flow diagram
- Fleet YAML visual editor
- Drag-and-drop pane arrangement
- Notification center (Telegram messages inline)
- Markdown/image preview in agent output

---

## 5. PTY Stream Bridging Detail

Current TUI path:
```
PTY master → pty_read_loop (8KB chunks)
           → crossbeam broadcast channel
           → subscriber thread
           → VTerm.process()
           → ratatui Buffer render
```

Proposed GUI path (WebSocket):
```
PTY master → pty_read_loop (8KB chunks)
           → crossbeam broadcast channel
           → subscriber thread
           → WebSocket frame (binary)
           → xterm.js write()
```

Key: xterm.js handles its own terminal emulation, so we skip VTerm entirely in the GUI path. Raw PTY output goes straight to xterm.js. This simplifies the bridge significantly.

The existing `framing.rs` protocol (1-byte tag + 4-byte length + payload) can be reused over WebSocket with minimal adaptation — just swap Unix socket transport for WebSocket transport.

---

## 6. Decision Framework

Answer these questions when ready to commit:

1. **Is core feature set stable enough?** If fleet/MCP/Telegram are still changing weekly, GUI work will constantly chase a moving target.
2. **Who is the target user?** If primarily developers who already use terminal tools, TUI may be sufficient. If targeting team leads, PMs, or demo audiences, GUI adds clear value.
3. **Is there bandwidth?** GUI is a multi-week commitment minimum. Half-done GUI is net negative.
4. **Can Phase 0 happen first?** Core extraction into workspace is valuable regardless of GUI decision. Consider doing it independently.

---

## 7. Minimum Viable GUI

If proceeding, the smallest useful GUI is:

> **Agent dashboard + embedded terminal**
> - List all agents with live state indicators
> - Click to open xterm.js terminal for any agent
> - Start/stop agents from UI
> - NO split panes, NO drag-drop, NO flow diagrams initially

This covers what TUI cannot do well (visual overview, click-to-interact) without duplicating what TUI already does well (terminal multiplexing, keyboard-driven workflow).
