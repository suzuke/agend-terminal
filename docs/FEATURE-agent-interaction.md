# Agent Interaction — Terminal Access to Agents

## Motivation

AgEnD agents run inside independent pseudo-terminals (PTYs). While most operations go through MCP tools and Telegram, sometimes you need to see the agent's terminal directly — watch what it's thinking, figure out where it's stuck, or type something to steer it.

AgEnD provides four CLI commands for this:

| Command | Purpose |
|---------|---------|
| `attach` | Connect to an agent's terminal, like tmux attach |
| `inject` | Send text into an agent's input |
| `list` | List all running agents |
| `kill` | Terminate an agent |

---

## attach — Connect to an Agent's Terminal

```
agend-terminal attach <agent-name>
```

`attach` connects your terminal to the specified agent's PTY. Once connected, you see the agent's live output and your keystrokes become its input. The experience is nearly identical to `tmux attach-session` or `screen -r`.

### Usage

```bash
# Connect to the agent named "dev"
agend-terminal attach dev

# With no name specified, defaults to "shell"
agend-terminal attach
```

### Detach

While attached, press **Ctrl+B** then **d** to detach.

This key combination matches tmux's default prefix: press Ctrl+B as the prefix key, release, then press d to trigger detach. After detaching, the agent continues running in the background and you can re-attach at any time.

### How It Works

1. attach connects to the daemon's bridge server over TCP
2. Authenticates using the API cookie
3. Switches the local terminal to raw mode (forwarding all keystrokes directly)
4. Starts a background thread to read and display agent PTY output
5. Main thread reads local keyboard input and forwards it to the agent
6. Automatically handles terminal resize events

On disconnect (Ctrl+B d or agent stops), terminal state is automatically restored.

### Notes

- Multiple attach sessions can connect to the same agent simultaneously (though inputs will compete)
- attach does not affect the agent's execution state — it's purely an observation and input channel
- If the agent is waiting for an interactive prompt (e.g., permission confirmation), you can respond directly through attach

---

## inject — Send Text to an Agent

```
agend-terminal inject <agent-name> <text...>
```

`inject` sends the specified text into the agent's PTY input, as if you typed it in the agent's terminal. Useful for automation or controlling agents from scripts.

### Usage

```bash
# Send text to the dev agent
agend-terminal inject dev "Please review this PR"

# Multiple words are joined with spaces
agend-terminal inject dev fix the bug in main.rs
```

### Injection Mechanism

inject has two modes:

**Normal mode (default):**
1. Writes text to the agent's PTY
2. Waits 50ms
3. Sends the submit key (typically Enter)

**Typed inject mode:**
- Used for system messages (e.g., `[AGEND-MSG]` prefix)
- Splits text into 64-byte chunks with 2ms delay between each byte
- Simulates human typing speed to prevent the backend's input buffer from overflowing

### Sanitization

inject automatically strips ANSI control sequences from the text, preventing ESC characters from disrupting the agent's terminal state.

### Return Value

On success, returns the number of injected bytes:

```json
{"ok": true, "result": {"bytes": 42}}
```

Returns an error if the agent doesn't exist or is restarting.

---

## list — List All Agents

```
agend-terminal list [--detailed] [--json]
```

Lists all agents running in the daemon.

### Output Modes

**Simple mode (default):**

```
$ agend-terminal list
lead
dev
reviewer
```

Simple mode reads port files from the run directory directly, so it works even when the daemon API is temporarily unresponsive.

**Detailed mode (`--detailed` or `-d`):**

```
$ agend-terminal list --detailed
NAME       BACKEND     STATE    HEALTH
lead       claude      ready    healthy
dev        claude      thinking healthy
reviewer   kiro-cli    idle     healthy
```

Detailed mode queries the daemon API for real-time status of each agent.

**JSON mode (`--json`):**

```bash
$ agend-terminal list --json
```

Outputs full JSON structure, suitable for scripts and automation tools. `--json` implies `--detailed`.

### Agent Status Fields

| Field | Description |
|-------|-------------|
| `agent_state` | Real-time state: `starting` / `ready` / `idle` / `thinking` / `tool_use` / `restarting` / `crashed` |
| `health_state` | Health state: `healthy` / `recovering` / `unstable` / `failed` / `hung` / `idle_long` / `paused` |
| `backend` | Backend name |
| `kind` | `managed` (daemon-managed) or `external` (externally connected) |

### Aliases

`list` has two aliases:

```bash
agend-terminal ls       # same as list
agend-terminal status   # same as list (backward compatible)
```

---

## kill — Terminate an Agent

```
agend-terminal kill <agent-name>
```

Forcefully terminates the specified agent process.

### Usage

```bash
# Terminate the dev agent
agend-terminal kill dev
```

### Termination Process

1. Validates the agent name format (`[a-zA-Z0-9_-]`)
2. Looks up the agent in the registry
3. Marks the agent state as `restarting`
4. Gets the subprocess PID
5. Calls `kill_process_tree(pid)` to terminate the entire process tree (including children)
6. As a fallback, also calls the PTY handle's `kill()` method
7. Records the event to the event log

### Auto-Restart

After `kill`, the daemon's health monitoring may automatically restart the agent (depending on current crash count and backoff state). To permanently stop an agent, remove it from fleet.yaml and restart the daemon, or use `agend-terminal stop` to stop the entire daemon.

### External Agents

For agents added via the `connect` command, `kill` removes them from the external registry. The actual process of external agents is not managed by the daemon.

---

## connect — Attach an External Agent

```
agend-terminal connect <name> --backend <backend> [--working-dir <path>] [-- extra-args...]
```

Connects a locally running agent to the daemon, giving it access to the daemon's MCP tools and communication features.

### Usage

```bash
# Connect a Claude Code instance
agend-terminal connect my-agent --backend claude --working-dir ~/Projects/foo

# Pass extra arguments to the backend
agend-terminal connect my-agent --backend gemini -- --model pro
```

### Differences from fleet.yaml

- Agents in fleet.yaml are **managed** (daemon spawns and manages their lifecycle)
- Agents added via `connect` are **external** (daemon provides tools only, no lifecycle management)
- External agents lack auto-restart, health monitoring, and related features

---

## Typical Workflows

### Scenario 1: Observing Agent Work

```bash
# Start the daemon
agend-terminal start

# Check all agent statuses
agend-terminal list --detailed

# Connect to the dev agent to watch what it's doing
agend-terminal attach dev

# Done watching, detach back to your own terminal
# (Press Ctrl+B, then d)
```

### Scenario 2: Manually Guiding an Agent

```bash
# Connect to the agent
agend-terminal attach lead

# Type directly in the agent's terminal to interact
# ...observe the agent's responses...

# Detach
# (Ctrl+B d)
```

### Scenario 3: Scripted Automation

```bash
#!/bin/bash

# Verify the agent is running
agend-terminal list --json | jq '.agents[] | select(.name == "dev")'

# Send an instruction to the agent
agend-terminal inject dev "Run cargo test and report the results"

# Check status after a while
sleep 30
agend-terminal list --detailed
```

### Scenario 4: Handling a Stuck Agent

```bash
# See which agent is stuck
agend-terminal list --detailed
# If you see health_state: hung

# Try attaching to see where it's stuck
agend-terminal attach dev

# Force restart if needed
agend-terminal kill dev
# The daemon will auto-restart the agent
```

---

## FAQ

### Q: No output after attaching?

Possible causes:
- Agent is in idle state waiting for input — try typing something
- Agent's PTY output is consumed by the backend's TUI — some backends use an alternate screen buffer, so attach may show a stale screen

### Q: Injected text wasn't executed by the agent?

Confirm the agent's state is `ready` or `idle`. If the agent is in `thinking` or `tool_use` state, injected text enters the PTY buffer but the agent may not process it immediately.

### Q: Agent auto-restarted after kill — how to prevent it?

The daemon's health monitor auto-restarts crashed agents by default. To permanently disable an agent, remove it from fleet.yaml and restart the daemon. Or simply `agend-terminal stop` to stop the entire daemon.

### Q: Can I attach to multiple agents at once?

Each `agend-terminal attach` command occupies one terminal. To watch multiple agents simultaneously, open multiple terminals and attach each to a different agent. Or use `agend-terminal app` to launch the TUI multi-pane interface, which displays all agents in a single screen.
