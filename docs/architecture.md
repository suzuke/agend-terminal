# AgEnD Terminal — Architecture Design Doc

> Rust rewrite of AgEnD: Agent Process Manager with direct PTY ownership.

## Overview

agend-terminal replaces the Node.js AgEnD + tmux stack with a single Rust binary that directly owns PTY master file descriptors. This eliminates the tmux dependency and the `send-keys` race condition at the architectural level.

```
┌─────────────────────────────────────────────────────┐
│                   agend-terminal daemon              │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐           │
│  │ Session 1│  │ Session 2│  │ Session N│  Core      │
│  │ PTY + FD │  │ PTY + FD │  │ PTY + FD │           │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘           │
│       │              │              │                 │
│  ┌────▼──────────────▼──────────────▼─────┐          │
│  │         Output Bus (broadcast)          │          │
│  │   drainer → log + ready + broadcast     │          │
│  └────┬──────────────┬──────────────┬─────┘          │
│       │              │              │                 │
│  ┌────▼────┐   ┌─────▼────┐  ┌─────▼────┐           │
│  │ Attach  │   │ Channel  │  │ Health   │           │
│  │ Client  │   │ Adapter  │  │ Monitor  │           │
│  └─────────┘   └──────────┘  └──────────┘           │
│                                                      │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐           │
│  │  Fleet   │  │  Comms   │  │ Context  │           │
│  │ Manager  │  │  Router  │  │ Rotation │           │
│  └──────────┘  └──────────┘  └──────────┘           │
│                                                      │
│  ┌────────────────────────────────────────┐          │
│  │         UDS API (Unix Domain Socket)    │          │
│  └────────────────────────────────────────┘          │
└─────────────────────────────────────────────────────┘
```

## Why Rust, Why Rewrite

| Problem (Node.js + tmux) | Root Cause | Rust Solution |
|---|---|---|
| `send-keys` race condition | tmux CLI is fire-and-forget, no atomicity | Direct `write(master_fd)` — kernel guarantees atomicity for ≤PIPE_BUF |
| Sequential restart bottleneck | tmux send-keys races force serialization | No tmux dependency, parallel spawn/kill |
| No output ownership | tmux owns the PTY, agend uses `pipe-pane` | Daemon owns master fd, reads output directly |
| MCP IPC complexity | Separate MCP server process + UDS + timeout | Agent communicates via CLI (`agend-terminal reply`) — same binary |
| Dual-path permission race | Terminal + Telegram both relay permissions | Single daemon routes all I/O, single source of truth |
| Context rotation fragility | Grace period hacks, rapid re-rotation prevention | Direct PTY output parsing, deterministic rotation |
| JS precision loss on snowflake IDs | JavaScript Number max safe integer | Rust u64 / i64, no precision issues |

---

## Module 1: Core — PTY Session Manager

**Status: Implemented (PoC + MVP)**

### Data Model

```rust
pub struct PtySession {
    pub id: u32,
    pub command: String,
    master: Box<dyn MasterPty>,       // PTY master — we own this
    writer: Box<dyn Write>,           // Atomic write path
    child: Box<dyn Child>,            // Child process handle
    exit_status: Option<i32>,         // Cached exit code
    size: (u16, u16),                 // Terminal cols x rows
    ready: AtomicBool,                // Ready pattern matched
    output_tx: broadcast::Sender,     // Fan-out to subscribers
    drainer_done: Notify,             // PTY EOF signal
}
```

### Key Operations

| Operation | Implementation |
|---|---|
| **spawn** | `portable-pty::openpty()` + `spawn_command()`. Env vars injected. Background drainer starts immediately. |
| **attach** | Client subscribes to `output_tx` broadcast. SIGWINCH sent to trigger redraw. |
| **detach** | Client unsubscribes. Session continues. Drainer keeps logging. |
| **inject** | `write_all(master_fd)` — atomic for payloads ≤ 4096 bytes (PIPE_BUF on macOS). Mutex serializes larger writes. |
| **kill** | Optional quit command → grace period → `child.kill()`. |
| **resize** | `master.resize()` → SIGWINCH to child. |

### Output Pipeline

```
PTY slave (child stdout) → master fd → drainer thread (spawn_blocking)
                                          ├── output.log (append)
                                          ├── ready pattern check (regex, 8KB window)
                                          └── broadcast::Sender → N subscribers
```

The drainer runs independently of attach state. Output is always captured.

### Concurrent Attach Behavior

- **Multi-attach read**: Supported. Broadcast channel fans output to all subscribers.
- **Multi-attach write**: Undefined — interleaved keystrokes from multiple clients. Production should add an "active writer" mode: one client writes, others are read-only viewers.

### Graceful Daemon Shutdown

**Status: Implemented.**

1. SIGTERM/SIGINT registered via `tokio::signal`.
2. Accept loop uses `tokio::select!` with signal channels.
3. On signal: kill all sessions in parallel (`child.kill()`), poll up to 5 seconds for exit.
4. Clean up socket file.
5. Attached clients receive connection close (EOF on their UDS read).

### Output Log Rotation

Single-session log (`output.log`) is append-only. For long-running sessions:
- **Context rotation**: Archives old log, starts fresh (see Module 6).
- **Size-based rotation**: When `output.log` exceeds 10 MB, rename to `output.log.1` and start new file. Keep at most 3 rotated logs per session.

---

## Module 2: Fleet — Multi-Agent Manager

### Config Format: `fleet.yaml`

Compatible with existing agend format, with extensions:

```yaml
defaults:
  backend: claude-code
  model: opus
  restart_policy:
    max_retries: 10
    backoff: exponential     # linear | exponential | fixed
    base_delay_seconds: 5
    max_delay_seconds: 300
  context_rotation:
    max_age_minutes: 180
    cooldown_minutes: 10
  health:
    idle_timeout_minutes: 60
    crash_respawn: true

instances:
  general:
    role: "Fleet coordinator"
    command: claude
    args: ["--model", "opus"]
    working_directory: ~/Documents/Hack/agend
    env:
      AGEND_INSTANCE_NAME: general
    telegram:
      topic_id: 12345
    ready_pattern: "All tools are now trusted"

  blog-writer:
    role: "Blog content writer"
    command: claude
    args: ["--model", "sonnet"]
    working_directory: ~/Documents/Hack/blog
    env:
      AGEND_INSTANCE_NAME: blog-writer
    ready_pattern: "ready"

teams:
  dev:
    members: [general, blog-writer]
```

### Fleet Manager

```rust
pub struct FleetManager {
    config: FleetConfig,
    sessions: HashMap<String, ManagedSession>,  // name → session
    config_watcher: notify::Watcher,            // inotify/kqueue
}

pub struct ManagedSession {
    session: Arc<PtySession>,
    config: InstanceConfig,
    health: HealthState,              // Owns all lifecycle state (see Module 5)
}
```

### Operations

- **`fleet start`**: Parse config → spawn all instances → wait for ready patterns.
- **`fleet stop`**: Graceful shutdown all (quit command → grace → kill).
- **`fleet restart <name>`**: Kill + respawn single instance, preserve session ID.
- **Hot reload**: `notify` crate watches `fleet.yaml`. On change:
  - New instances → spawn
  - Removed instances → kill
  - Changed config → restart affected instances
  - Unchanged → no-op

### vs Node.js

| Node.js | Rust |
|---|---|
| FleetManager creates tmux windows | FleetManager spawns PTY sessions directly |
| Config reload requires manual restart | `notify` crate watches file, hot reload |
| Sequential shutdown to avoid race | Parallel shutdown, no races |

---

## Module 3: Communication — Agent Messaging

### Design Principle

Agents communicate via **CLI commands** (Bash tool), not MCP. This eliminates the MCP server process, the IPC bridge, and the timeout complexity.

### Agent-side Interface

Agents use `agend-terminal` as a Bash tool:

```bash
# Reply to the user who messaged this agent
agend-terminal reply "Here's the result..."

# Send a message to another agent
agend-terminal send general "Task complete, PR ready"

# Send with metadata
agend-terminal send general --kind report --correlation-id abc123 "Done"

# React to the last message
agend-terminal react thumbsup

# Read pending messages (non-blocking)
agend-terminal inbox
```

### How It Works

```
Agent (claude-code)
  │ Bash tool: `agend-terminal reply "hello"`
  ▼
agend-terminal CLI
  │ connects to daemon UDS
  │ sends Request::Reply { text: "hello" }
  ▼
Daemon
  │ looks up which session sent this (by UDS peer)
  │ routes to appropriate channel adapter
  ▼
Telegram / Discord / other
```

### Protocol Extensions

```rust
enum Request {
    // ... existing ...

    /// Agent sends a reply to the user
    Reply { text: String },
    /// Agent sends a message to another instance
    Send {
        target: String,
        text: String,
        kind: Option<MessageKind>,        // query | task | report | update
        correlation_id: Option<String>,
    },
    /// Agent reads its inbox
    Inbox,
    /// Agent reacts to a message
    React { emoji: String },
}

enum Response {
    // ... existing ...
    Sent { message_id: String },
    Messages { messages: Vec<InboxMessage> },
}
```

### Agent Instructions

Each agent's system prompt includes:

```
## Communication
Use Bash tool to communicate:
- `agend-terminal reply "text"` — respond to the user
- `agend-terminal send <target> "text"` — message another agent
- `agend-terminal inbox` — check for new messages

Do NOT use the reply tool or MCP tools for communication.
```

### vs Node.js (MCP)

| Node.js MCP | Rust CLI |
|---|---|
| Separate MCP server process per instance | Same binary, UDS call |
| 30s/60s timeout handling | Instant UDS response |
| 20+ MCP tools to maintain | ~5 CLI subcommands |
| MCP protocol overhead (JSON-RPC) | Length-prefixed JSON, minimal overhead |
| Tool registration, schema sync | No registration needed — it's a shell command |
| Agents need MCP tool definitions | Agents use Bash tool (always available) |

### Message Routing

```
                    ┌──────────────┐
                    │  Daemon      │
                    │  Message     │
Telegram ──────────▶│  Router      │──────────▶ Agent PTY (inject)
Discord  ──────────▶│              │
Agent CLI ─────────▶│              │──────────▶ Telegram (send)
                    └──────────────┘──────────▶ Other Agent (inject)
```

The router maintains a mapping: `instance_name → (session_id, channel_config)`.

### Message Delivery Semantics

**Known limitation:** When an agent is busy (executing a tool call), multiple injected messages accumulate in the PTY stdin buffer. The agent reads them as a single block of text after finishing its current turn, potentially treating multiple messages as one.

MCP does not have this problem because each message is a discrete tool call with structured boundaries.

**Mitigation — Notification + Pull model (recommended):**

Instead of injecting the full message text, inject a short notification:

```
[New message from user:alice. Run: agend-terminal inbox]
```

The agent then calls `agend-terminal inbox` to retrieve all pending messages as structured data. This ensures:
- Each message is individually addressable
- Agent processes messages one at a time
- No parsing ambiguity from concatenated messages

The daemon maintains a per-session message queue (bounded, in-memory). `inbox` drains the queue and returns structured JSON.

```rust
struct MessageQueue {
    messages: VecDeque<InboxMessage>,
    max_size: usize,  // drop oldest when full
}
```

**Fallback — Delimiter protocol:**

If direct inject is needed (e.g., for simpler agents), wrap each message:

```
---AGEND_MSG---
[user:alice] Do task A
---AGEND_MSG---
```

Agent system prompt teaches it to split on `---AGEND_MSG---`.

---

## Module 4: Channel — Communication Platform Adapters

### Architecture

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    /// Start receiving messages. Calls `on_message` for each.
    async fn start(&self, tx: mpsc::Sender<IncomingMessage>) -> Result<()>;
    /// Send a message to a channel/topic.
    async fn send(&self, target: ChannelTarget, text: String) -> Result<String>;
    /// React to a message.
    async fn react(&self, message_id: &str, emoji: &str) -> Result<()>;
    /// Edit a previously sent message.
    async fn edit(&self, message_id: &str, new_text: String) -> Result<()>;
}
```

### Telegram Adapter

```rust
pub struct TelegramAdapter {
    bot: teloxide::Bot,
    chat_id: ChatId,
    topic_map: HashMap<String, i32>,  // instance_name → topic_id
}
```

**Inbound flow:**
```
Telegram → teloxide webhook/polling → IncomingMessage
  → daemon routes by topic_id → find instance
  → format: "[user:username] message text"
  → inject into agent's PTY
```

**Outbound flow:**
```
Agent calls `agend-terminal reply "text"`
  → daemon receives Reply request
  → looks up instance's Telegram topic_id
  → TelegramAdapter.send(topic_id, text)
```

### Message Format

Inbound messages injected into PTY as the agent's stdin. The format must be parseable by the agent:

```
[user:chiachenghuang] 請幫我 review 這個 PR
```

For inter-agent messages:

```
[from:general] 請處理這個 task
```

### Discord Adapter (Future)

Same `ChannelAdapter` trait, different implementation. Discord threads map to instances like Telegram topics.

### vs Node.js

| Node.js | Rust |
|---|---|
| Telegram bot in JS (telegraf/grammy) | teloxide (Rust-native, async) |
| MCP tools bridge messages | Direct PTY inject, no bridge |
| Message queue in memory | mpsc channel, backpressure built-in |

---

## Module 5: Health — Process Lifecycle

### Ownership: Health Monitor replaces Reaper

In Phase 1 (current), `spawn_session_reaper` handles session exit — it simply removes the session from the HashMap. **In Phase 4, Health Monitor takes over as the sole session exit handler.** The reaper is removed.

Decision flow on session exit:
```
session exits → Health Monitor receives drainer_done
  → Check restart policy (from Fleet config)
  → If should_restart:
      → Increment crash_count, calculate backoff
      → Wait backoff delay
      → Spawn new session (same config), update HashMap
  → If not (max_retries exceeded, or manual kill):
      → Remove from HashMap, log reaped
```

### Data Model

```rust
/// Owned by Health Monitor — single source of truth for lifecycle state.
pub struct HealthState {
    last_output: Instant,
    crash_count: u32,
    last_crash: Option<Instant>,
    policy: RestartPolicy,          // Set by Fleet Manager
    stable_since: Option<Instant>,  // For crash count reset
}
```

Fleet Manager sets the `RestartPolicy` (max_retries, backoff params). Health Monitor owns the execution state (crash_count, last_crash). No duplication.

### Crash Detection

**Event-driven** — no polling:

```
Drainer EOF → drainer_done.notify()
  → HealthMonitor receives notification
  → Check exit code
  → If unexpected exit (non-zero, or session should be long-running):
      → Increment crash_count
      → Calculate backoff delay
      → Schedule respawn
```

### Restart Policy

```rust
pub struct RestartPolicy {
    max_retries: u32,              // 0 = infinite
    backoff: BackoffStrategy,      // exponential, linear, fixed
    base_delay: Duration,
    max_delay: Duration,
    reset_after: Duration,         // Reset crash count after stable period
}
```

Backoff calculation:
- **exponential**: `min(base * 2^n, max_delay)`
- **linear**: `min(base * n, max_delay)`
- **fixed**: `base`

After `reset_after` of stable running, crash count resets to 0.

### Idle Detection

The drainer already timestamps every output chunk. HealthMonitor subscribes to the broadcast channel and updates `last_output`. If no output for `idle_timeout`, the session is marked idle.

Idle sessions can trigger:
- Notification to fleet manager
- Optional auto-restart (for context rotation)
- Alert to Telegram

### vs Node.js

| Node.js | Rust |
|---|---|
| Poll-based crash detection (check tmux window) | Event-driven via drainer_done Notify |
| Restart delay in JS setTimeout | tokio::time::sleep, precise |
| Idle check by polling tmux output | Broadcast subscriber, zero-cost when active |

---

## Module 6: Context — Context Rotation

### The Problem

LLM agents (Claude Code, etc.) have finite context windows. When context fills up, the agent becomes less effective. AgEnD needs to detect this and restart the session.

### Detection Strategy

Parse PTY output for context usage indicators. Claude Code outputs context usage in its status line:

```
Context: 85% used (170K/200K tokens)
```

```rust
pub struct ContextMonitor {
    session_id: u32,
    max_age: Duration,              // Hard limit: restart after N minutes
    usage_threshold: f32,           // 0.0-1.0, trigger at this usage level
    cooldown: Duration,             // Min time between rotations
    last_rotation: Option<Instant>,
    usage_pattern: Regex,           // Configurable per-backend in fleet.yaml
}
```

The `usage_pattern` regex is configurable per instance in `fleet.yaml` via `context_pattern`, since different backends (Claude Code, Codex, Gemini CLI) output context usage in different formats:

```yaml
instances:
  general:
    context_rotation:
      context_pattern: "Context:\\s+(\\d+)%"   # Claude Code format
  codex-agent:
    context_rotation:
      context_pattern: "tokens:\\s+(\\d+)/(\\d+)"  # Codex format
```

### Rotation Flow

```
ContextMonitor subscribes to output broadcast
  → Regex matches context usage line
  → If usage > threshold OR age > max_age:
      → Check cooldown (prevent rapid re-rotation)
      → Save session state (working directory, env vars)
      → Kill session (graceful: inject /exit)
      → Spawn new session with same config
      → Log rotation event
```

### Session Continuity

On rotation, the new session inherits:
- Same working directory
- Same environment variables
- Same Telegram topic mapping
- Previous session's output log (archived, new log started)

The agent starts fresh but the fleet config ensures it gets the same role/instructions.

### vs Node.js

| Node.js | Rust |
|---|---|
| Grace period hacks (10 min cooldown) | Deterministic: cooldown + usage threshold |
| Parse context from tmux capture-pane | Parse from broadcast stream (already have it) |
| Restart via tmux kill-window + new-window | Kill session + spawn new PtySession |

---

## Module 7: CLI — User Interface

### Command Tree

```
agend-terminal
├── daemon                           # Start the daemon
│   └── --config fleet.yaml          # Load fleet config
│
├── fleet                            # Fleet management
│   ├── start [name...]              # Start all or specific instances
│   ├── stop [name...]               # Graceful stop
│   ├── restart [name...]            # Restart instances
│   └── status                       # Fleet overview
│
├── ls                               # List sessions (short form)
├── attach <id|name>                 # Attach to session
├── logs <id|name>                   # Tail output log
│   └── --follow                     # Follow mode (like tail -f)
│
├── inject <id|name> <text>          # Send raw input
├── kill <id|name>                   # Kill session
│   ├── --quit-cmd CMD               # Graceful quit command
│   └── --grace N                    # Grace period seconds
│
├── spawn [flags] <cmd> [-- args]    # Manual spawn (outside fleet)
│   ├── --env KEY=VALUE
│   ├── --cols N --rows N
│   └── --ready-pattern REGEX
│
├── reply <text>                     # Agent: reply to user (from PTY)
├── send <target> <text>             # Agent: message another instance
│   ├── --kind query|task|report
│   └── --correlation-id ID
├── inbox                            # Agent: read pending messages
└── react <emoji>                    # Agent: react to message
```

### Argument Parsing

Use `clap` crate for structured argument parsing (replace current manual parsing):

```rust
#[derive(Parser)]
#[command(name = "agend-terminal")]
enum Cli {
    Daemon { #[arg(long)] config: Option<PathBuf> },
    Fleet { #[command(subcommand)] cmd: FleetCmd },
    Ls,
    Attach { target: String },
    Logs { target: String, #[arg(long)] follow: bool },
    Inject { target: String, text: Vec<String> },
    Kill { target: String, #[arg(long)] quit_cmd: Option<String> },
    Spawn { /* flags */ },
    Reply { text: Vec<String> },
    Send { target: String, text: Vec<String> },
    Inbox,
    React { emoji: String },
}
```

---

## Cross-Cutting: UDS API Design

All communication goes through a single Unix domain socket.

### Request Routing

The daemon identifies the caller via session token:
- **External CLI** (user typed `agend-terminal ls`): No `AGEND_SESSION_ID` env var → no session context.
- **Agent CLI** (agent's Bash tool ran `agend-terminal reply`): `AGEND_SESSION_ID` env var identifies the session → daemon routes accordingly.

### Session Token

On spawn, the daemon sets `AGEND_SESSION_ID=<id>` in the child's environment. When the agent runs `agend-terminal reply "text"`, the CLI reads `AGEND_SESSION_ID` and includes it in the request. The daemon uses this to route the reply to the correct channel.

```rust
// In spawn:
cmd.env("AGEND_SESSION_ID", id.to_string());

// In CLI (reply/send):
let session_id = std::env::var("AGEND_SESSION_ID")
    .context("Not running inside an agend-terminal session")?;
```

**Security note:** `AGEND_SESSION_ID` is inherited by all child processes within the session. Any process can read it and impersonate the agent via `agend-terminal reply`. This is acceptable for a single-user local tool. For multi-host deployments (Phase 5), a stronger auth mechanism (e.g., per-session HMAC token verified by the daemon) would be needed.

---

## Data Flow Summary

### User sends message via Telegram

```
Telegram → TelegramAdapter.on_message()
  → Router: lookup topic_id → instance "general"
  → Format: "[user:chiachenghuang] message text"
  → session.write_input(formatted_message)
  → Agent reads from stdin, processes, calls Bash tool
  → `agend-terminal reply "response"`
  → CLI connects to daemon UDS, sends Reply request
  → Router: session 1 → Telegram topic 12345
  → TelegramAdapter.send(topic_id, "response")
  → User sees response in Telegram
```

### Agent-to-agent communication

```
Agent A (blog-writer) runs: `agend-terminal send general "PR ready"`
  → CLI reads AGEND_SESSION_ID=2
  → Connects to daemon UDS
  → Request::Send { target: "general", text: "PR ready" }
  → Router: find session for "general" → session 1
  → Format: "[from:blog-writer] PR ready"
  → session_1.write_input(formatted_message)
  → Agent "general" reads from stdin, processes
```

---

## Implementation Phases

### Phase 1: Core Hardening (Current)
- [x] PTY spawn/attach/detach/inject/kill
- [x] Output capture + ready detection
- [x] Graceful daemon shutdown
- [ ] `clap` CLI parsing (replace manual arg parsing)
- [ ] Session token (`AGEND_SESSION_ID`)

### Phase 2: Fleet + Communication
- [ ] `fleet.yaml` parser
- [ ] FleetManager: start/stop/restart
- [ ] `reply` / `send` / `inbox` CLI commands
- [ ] Message router
- [ ] Config hot-reload (`notify` crate)

### Phase 3: Channel Integration
- [ ] Telegram adapter (teloxide)
- [ ] Inbound: Telegram → PTY inject
- [ ] Outbound: Agent reply → Telegram send
- [ ] Topic-to-instance mapping

### Phase 4: Health + Context
- [ ] Health Monitor replaces `spawn_session_reaper`
- [ ] Event-driven crash detection
- [ ] Restart policy (exponential backoff)
- [ ] Context usage parsing
- [ ] Auto-rotation with cooldown
- [ ] Idle detection

### Phase 5: Production
- [ ] Discord adapter
- [ ] SQLite persistence (schedules, cost tracking)
- [ ] Web dashboard (optional)
- [ ] Multi-host fleet (SSH-based remote spawn)

---

## Dependency Plan

```toml
[dependencies]
# Core (already)
portable-pty = "0.8"
tokio = { version = "1", features = ["full"] }
nix = "0.29"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
regex = "1"

# Phase 1
clap = { version = "4", features = ["derive"] }

# Phase 2
serde_yaml = "0.9"          # fleet.yaml parsing
notify = "7"                 # Config file watching

# Phase 3
teloxide = "0.13"            # Telegram bot
reqwest = "0.12"             # HTTP client (for webhooks)

# Phase 4
rusqlite = "0.32"            # SQLite (schedules, logs)
```
