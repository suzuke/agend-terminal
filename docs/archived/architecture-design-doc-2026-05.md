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
│  │         TCP API (localhost NDJSON)    │          │
│  └────────────────────────────────────────┘          │
└─────────────────────────────────────────────────────┘
```

## Why Rust, Why Rewrite

| Problem (Node.js + tmux) | Root Cause | Rust Solution |
|---|---|---|
| `send-keys` race condition | tmux CLI is fire-and-forget, no atomicity | Direct `write(master_fd)` — kernel guarantees atomicity for ≤PIPE_BUF |
| Sequential restart bottleneck | tmux send-keys races force serialization | No tmux dependency, parallel spawn/kill |
| No output ownership | tmux owns the PTY, agend uses `pipe-pane` | Daemon owns master fd, reads output directly |
| MCP IPC complexity | Separate MCP server process + TCP API + timeout | Agent communicates via CLI (`agend-terminal reply`) — same binary |
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
5. Attached clients receive connection close (EOF on their TCP read).

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
- **fleet.yaml edits require a daemon restart** (Sprint 29 PR-6). The hot-reload diff engine was removed — KISS for a single-user dev tool where daemon restart costs ~5 seconds. Operators edit `fleet.yaml`, then stop and re-launch the daemon. All agents respawn with the new config on next start.

### vs Node.js

| Node.js | Rust |
|---|---|
| FleetManager creates tmux windows | FleetManager spawns PTY sessions directly |
| Config reload requires manual restart | Same — fleet.yaml edits need daemon restart |
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
  │ connects to daemon TCP API
  │ sends Request::Reply { text: "hello" }
  ▼
Daemon
  │ looks up which session sent this (by auth cookie)
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
| Separate MCP server process per instance | Same binary, TCP API call |
| 30s/60s timeout handling | Instant TCP API response |
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

## Module 8: Self-Healing — Supervised Hot Upgrade

Zero-downtime daemon upgrades with automatic rollback on failure. Unix-only
(the socket-swap + symlink-rename trick doesn't map cleanly to Windows;
`agend-terminal upgrade` returns an error there).

### Layering

```text
agend-supervisor             ← frozen, tiny, upgraded rarely
  └── agend-terminal daemon  ← hot-swappable binary (PTY master owner)
        └── agent PTYs       ← spawned fresh by the new daemon after upgrade
```

The supervisor's job is narrow on purpose: own the daemon child, orchestrate
binary swaps, and roll back on failure. It does not own PTYs, Telegram state,
or the MCP server — if it did, every supervisor change would force agents to
restart. Keeping its surface small is the whole point: it can stay frozen for
months while the daemon iterates daily.

### Filesystem Layout

```text
$AGEND_HOME/
  bin/
    current          → symlink to the active daemon binary
    prev             → symlink to the previous binary (rollback target)
    store/
      <sha256>       ← content-addressed binaries staged by `upgrade`
    supervisor       ← symlink to the installed supervisor binary
  run/
    <pid>/           ← per-daemon run dir (unchanged from Module 1)
    supervisor.sock  ← supervisor IPC socket (0600)
    supervisor.pid   ← supervisor's PID
    upgrade-marker   ← JSON blob written pre-upgrade, consumed by new daemon
```

Content-addressed store means swapping a binary is just a `rename(2)` over a
symlink — atomic, no half-written state. Rolling back is the same operation
in reverse.

### Protocol: `supervisor.sock`

NDJSON over localhost TCP. One request, streaming progress frames, terminal response.

| Request                    | Sent by                      | Purpose                                    |
|----------------------------|------------------------------|--------------------------------------------|
| `Ping`                     | CLI pre-upgrade probe        | Detect a live supervisor + fetch its PID  |
| `Status`                   | CLI                          | Query current daemon version               |
| `Upgrade { new_hash, … }`  | CLI                          | Run the upgrade workflow                   |
| `Ready { pid, version }`   | Daemon post-boot             | "I'm up" ping consumed by `wait_for_ready` |
| `ShuttingDown { reason }`  | Daemon pre-exit (optional)   | Distinguishes clean stop from crash        |

Wire version is pinned at `1`; CLI refuses servers with a newer version and
tells the user to upgrade the client. The protocol is deliberately not
backwards-compatible in the degrade-gracefully sense — the supervisor is the
oldest moving piece and will outlive its peers.

### Upgrade Flow

```text
CLI                             Supervisor                      Daemon (old)   Daemon (new)
 │                                  │                                │              │
 │ 1. stage new binary in store/   │                                │              │
 │ 2. stage current  → prev/       │                                │              │
 │ 3. swap bin/current symlink     │                                │              │
 │ 4. Upgrade{ new_hash, … } ───► │                                │              │
 │                                  │ 5. self-test (AGEND_SELF_TEST) │              │
 │                                  │    spawn new binary, wait exit │              │
 │                                  │ 6. write upgrade-marker        │              │
 │                                  │ 7. SIGTERM old ──────────────► │ exit         │
 │                                  │ 8. spawn new ────────────────────────────────►│
 │                                  │ 9. wait for Ready ping ◄──────────────────────│
 │                                  │ 10. stability window (60s)     │              │
 │                                  │     — watch for crashes —      │              │
 │ 11. ◄────── Ok { final: true }  │                                │              │
```

If any step after 5 fails — self-test exits non-zero, ready ping doesn't
arrive within the timeout, or the new daemon crashes ≥ 2 times inside the
stability window — the supervisor rolls back:

```text
Supervisor: stop new daemon → swap bin/current → store/<prev_hash>
            → delete upgrade-marker → respawn old daemon → report rollback
```

The CLI surfaces this as a hard failure (`Err`) even though the system is back
in a good state. "Rollback succeeded" is still a failed upgrade from the
user's perspective.

### Why Agents Restart

The daemon owns every agent's PTY master fd. When it exits, those fds close
and the child processes see EOF / SIGHUP. CRIU-style fd handoff was considered
and rejected: platform-fragile, complicates the supervisor's freeze surface,
and the existing crash-respawn code path already handles "daemon went away,
agents came back" correctly. The MVP trades a few seconds of agent
interruption for a much simpler supervisor.

After the new daemon boots, it reads `run/upgrade-marker` and — if present —
injects `[system] Daemon upgraded from vX to vY. All agents restarted.` into
every agent's PTY instead of the normal crash-respawn notice. The marker is
then deleted so a subsequent unrelated crash respawn gets its real reason.

### Rollback Triggers

- **Self-test failure**: new binary exits non-zero under `AGEND_SELF_TEST=1`
  before we kill the old daemon. Cheapest failure mode — zero disruption.
- **Ready-ping timeout**: new daemon exec'd but never pinged within
  `--ready-timeout-secs` (default 60). Catches hangs, missing deps, busted
  config.
- **Stability-window crash**: new daemon pings Ready but then crashes ≥ 2
  times within `--stability-secs` (default 60). Catches "boots fine, dies
  under first real request" regressions.

### Bootstrap Migration

Fresh installs with no supervisor: the first `agend-terminal upgrade
--install-supervisor --yes` lays down `bin/current`, `bin/supervisor`, and
`bin/store/`, then tells the user to start `agend-supervisor`. It does **not**
auto-start the supervisor — how to daemonize (nohup, systemd, launchd) is a
policy decision the installer's shell owns.

### Testable Surface

- `supervisor::client` — pure (sha256, symlink swap, TCP send/recv).
- `supervisor::ipc` — serde roundtrip tests on every Request/Response variant.
- `supervisor::self_test` — passes with valid `fleet.yaml`, fails on corrupt.
- `supervisor::server` — end-to-end integration tests in
  `tests/self_healing_supervisor.rs` drive the real `agend-supervisor` binary
  against a temp `$AGEND_HOME`, using `src/bin/agend-mock-daemon.rs` as the
  daemon child. Two cases covered:
  - **Success path**: v1 booted → stage v2 → `Upgrade` → terminal `Ok` →
    `current` repointed at v2 → v2 sentinel observed.
  - **Rollback path**: crash counter armed at 2 so the new daemon crashes
    post-ready twice inside the stability window → supervisor repoints
    `current` back at v1, deletes the upgrade marker, and responds `Err`.
    This path also exercises the watcher's `waitpid(WNOHANG)` zombie reap —
    pure `kill(pid, 0)` liveness probing would leave crashed children as
    zombies and silently report the upgrade as stable.

  The v2 binary is fabricated by appending padding bytes to the v1
  mock-daemon so the sha256 differs while behaviour stays identical
  (ELF/Mach-O loaders ignore trailing bytes). The mock daemon is
  Unix-only and never shipped — it lives under `src/bin/` purely so
  `cargo test` builds it into `target/debug/` alongside the supervisor.

---

## Cross-Cutting: Daemon API (IPC)

The daemon exposes a localhost-only TCP API using NDJSON (newline-delimited JSON) protocol.

### Transport

- **Localhost TCP** on a random port (published to `<home>/run/<pid>/api.port`)
- **No TLS** — localhost-only assumption; same-user access enforced by filesystem permissions
- **Connection cap**: 32 concurrent sessions (fixed const; #env-cleanup: the `AGEND_API_MAX_CONNS` override was demoted)

### Authentication

1. Daemon issues a 32-byte random cookie at startup (`<home>/run/<pid>/api.cookie`, mode 0600)
2. Client reads the cookie file and sends `{"auth":"<hex>"}` as the first NDJSON line
3. Daemon verifies via constant-time comparison
4. **Pre-auth timeout**: 5s — prevents slow-loris holding connection slots

### Protocol

After auth handshake, each line is a JSON request; daemon responds with one JSON line per request:

```
→ {"method": "list"}
← {"ok": true, "result": {"agents": [...]}}

→ {"method": "inject", "params": {"name": "dev-1", "data": "hello"}}
← {"ok": true, "result": {"bytes": 5}}
```

### Agent Identity

On spawn, the daemon sets `AGEND_INSTANCE_NAME=<name>` in the child's environment. MCP tool calls from the agent include this identity automatically. The daemon uses it to route replies and track per-agent state.

---

## Cross-Cutting: CI Watch & Dispatch Chains

### CI watch file identity (#942 / #943)

Each `ci action=watch` subscription persists a JSON file at
`<home>/ci-watches/<filename>.json`. The filename has been hardened
twice in quick succession:

- **#942 — `canonicalize_repo_slug`.** Operators (and the bridge)
  refer to the same repo in seven divergent forms:
  `git@github.com:owner/repo.git`, `https://github.com/owner/repo`,
  `https://github.com/owner/repo.git`, `git+ssh://...`, raw
  `owner/repo`, with `.git` suffix, with case variation. Pre-#942
  each form computed its own watch filename → duplicate subscriptions
  with divergent state → notifications fanned out N times. Post-#942
  the slug is canonicalized to `owner/repo` (lowercase, no `.git`,
  no scheme) before hashing.
- **#943 — sha256 hash.** The pre-#943 filename used Rust's
  `DefaultHasher` (SipHash-2-4 truncated to 64 bits → 16 hex chars).
  Collision-grade is `~2^32` brute force — too thin for an identity
  key the operator relies on. Replaced with sha256 (256-bit, 64 hex
  chars). ~900 ns per call vs ~100 ns; at typical
  ~100 subscriptions/agent/day the ~90 µs/day delta is negligible.

The two changes ship together as a unit; the canonicalization
narrows divergence at the *input* level while sha256 widens the
hash *output* to remove the collision-grade concern at the same
time.

**Legacy migration (#942/#943 PR-B).** `bootstrap::prepare` runs
`migrate_legacy_watch_filenames` synchronously at boot. The migration
scans `<home>/ci-watches/*.json`, identifies non-canonical filenames
(stem length ≠ 64), reads each body to recover `repo` + `branch`,
canonicalizes the `repo` field, computes the new sha256 filename, and
renames the file. Conflicts (two old files mapping to the same target)
are logged with the FIRST one winning; the operator can hand-resolve.
Idempotent — re-running on already-migrated state is a no-op. The
synchronous-at-boot ordering means the poll loop never sees the old
files, so operators don't observe duplicate 72 h notifications across
the transition.

### CI watch survives bind/release handoff (#931)

The daemon's `release_worktree` path used to unsubscribe the releasing
agent from every `ci-watch` they held. If that agent was the **sole**
subscriber, the watch file's `next_after_ci` chain and polling state
were destroyed, and the chained handoff (`ci pass → next_after_ci →
[ci-ready-for-action] to reviewer`) never fired.

Post-#931 the file is kept intact on the sole-subscriber release path:
`next_after_ci`, `last_notified_head_sha`, and the polling state all
survive. The next agent that binds the same branch (via
`dispatch_auto_bind_lease`) inherits the chain and the handoff fires
on the next CI green tick.

The reviewer-dispatch chain `dev → ci → reviewer` is the canonical
example; the same fix unblocks any multi-agent workflow where one
agent's release precedes another's bind.

### Correlation IDs on `system:ci` and dispatch_idle (#946 / #947)

Two correlation_id sources flow into inbox routing:

- **#946 — `system:ci` notifications.** Every `system:ci` enqueue
  (pass, fail, stalled, conflict, etc.) now carries
  `correlation_id = "{repo}@{branch}"` — a stable identifier per
  watched branch. Pre-#946 these were `None`, leaving operators no
  way to filter inbox dumps to a single branch. Greppable example:
  `grep '"correlation_id":"owner/repo@feat/x"' $AGEND_HOME/inbox/*.jsonl`.
- **#947 — dispatch_idle watchdog fallback.** When the watchdog fires
  on a `kind=task`/`kind=query` send whose upstream had no
  `correlation_id`, the synthesized fallback uses the canonical
  dispatch id format `disp-<unix_micros>-<seq>`. Pre-#947 the
  fallback was either `None` or a churning per-call ULID with no
  cross-message tie. The new format is self-documenting via prefix
  and stable across the dispatch + the eventual report.

### Test infrastructure — deterministic primitives

Two helpers extracted from in-test repetition (SOP 1 §3.20):

- **`admin::cleanup_zombies::poll_until_dead(pid, timeout)`** (#934).
  `pub(crate)` deterministic alternative to `thread::sleep(N)`
  patterns. Polls `kill -0` (Unix) / `OpenProcess` (Windows) every
  10 ms up to `timeout`. Returns `bool` (dead-on-time). Already
  consumed by `agent.rs` shutdown path + `process.rs` reaper.
- **`api::handlers::instance::await_sentinel_nonempty`** (#949). Polls
  a sentinel file path until it has non-empty content or the timeout
  elapses. Contract clarified by the rename: pre-#949 the helper was
  named for the *file* existing, but `instance` codepaths needed the
  *content* to be present (the file is created empty then written to).
  Tests now use the helper at the four `instance.rs` boot-sentinel
  sites; CI-side flake is gone.

Both helpers are `pub(crate)` — call sites stay in-crate, and the
helpers exist primarily to make tests deterministic without falling
back to sleep tuning.

---

## Cross-Cutting: Daemon Notification & State Helpers

### `notify_system` helper (#1335)

Daemon modules that emit inbox notifications (watchdogs, timeouts, idle
detectors) previously duplicated ~8 lines of `InboxMessage::new_system` +
builder chain + `enqueue_with_idle_hint`. The `notify_system()` helper
collapses this to a single call:

```rust
crate::inbox::notify_system(
    home,
    target,          // recipient agent name
    "system:source", // source identifier
    "event-kind",    // inbox message kind
    body,            // message body (impl Into<String>)
    correlation_id,  // Option<&str>
    task_id,         // Option<&str>
);
```

All `notify_system` messages use `delivery_mode("inbox_fallback")`. Modules
that need different delivery modes (e.g. `cron_tick`, `daemon/mod.rs`
schedule/query) should continue using `InboxMessage` directly.

### Event bus (#1336)

Feature-flagged global event bus for decoupled daemon-internal signaling.

**Enable**: `AGEND_EVENT_BUS=1` environment variable.

**Usage**:

```rust
// Zero-cost when disabled — closure never called.
event_bus::emit_lazy("pr_state.merged", || {
    json!({ "repo": repo, "branch": branch })
});

// Guard for callers that build expensive payloads.
if event_bus::is_enabled() {
    let payload = expensive_computation();
    event_bus::emit("custom.event", payload);
}
```

When disabled (default), `emit_lazy` returns immediately without allocating
or evaluating the closure. `is_enabled()` is a single `AtomicBool` load.

### `with_pr_state` flock helper (#1342)

All mutations to `pr-state/*.json` files must go through `with_pr_state()`
or `with_pr_state_or_create()`. These helpers:

1. Acquire an `fs4` file lock on `<filename>.lock`
2. Read + deserialize the current `PrState`
3. Run the caller's mutation closure
4. Serialize + `atomic_write` the result

```rust
// Mutate existing state (returns None if file doesn't exist).
let result = with_pr_state(home, repo, branch, |state| {
    state.ready_emitted_for_sha = Some(state.head_sha.clone());
    ScanAction::Saved
});

// Create if absent (uses default_fn for initial state).
with_pr_state_or_create(home, repo, branch, default_fn, |state| {
    state.ci_results.push(result);
});
```

This eliminates the lost-update race where concurrent writers (scanner tick
+ gh-poll) could overwrite each other's changes. The lock file is separate
from the data file to avoid holding a lock on a file being atomically
replaced.

### Auto-release worktree on pr-merged (#1344)

When the scanner detects `MergeState::Merged`, it calls
`auto_release_for_merged_branch(home, &state.branch)` **before** emitting
the `[pr-merged]` inbox notification. This function:

1. Scans `runtime/<agent>/binding.json` for agents bound to the branch
2. Acquires the binding lock
3. Checks `is_worktree_clean` (dirty worktrees are skipped with a warning)
4. Calls `release_full` to remove the worktree + clear the binding

This ensures `gh pr merge --delete-branch` succeeds because no local
worktree holds the branch ref.

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
  → CLI connects to daemon TCP API, sends Reply request
  → Router: session 1 → Telegram topic 12345
  → TelegramAdapter.send(topic_id, "response")
  → User sees response in Telegram
```

### Agent-to-agent communication

```
Agent A (blog-writer) runs: `agend-terminal send general "PR ready"`
  → CLI authenticates via auth cookie
  → Connects to daemon TCP API
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
- [x] `clap` CLI parsing (`Commands` enum in `src/main.rs`)
- [ ] Auth cookie (`<home>/run/<pid>/api.cookie`)

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

---

## Source Layout (reference)

The module design above is aspirational and largely file-agnostic; the table below is the current file-level mapping as of Sprint 61 (2026-05). See `CONTRIBUTING.md` for the contribution-time style rules.

| Responsibility | File(s) |
|---|---|
| PTY session management, spawn, inject, ready detection | `src/agent.rs` |
| Backend presets (Claude, Kiro, Codex, Gemini, OpenCode, Shell) | `src/backend.rs` |
| Shared helpers (messaging, fleet mutation, branch validation) | `src/agent_ops.rs` |
| Daemon JSON control API (wire protocol + handlers) over TCP loopback | `src/api/mod.rs`, `src/api/handlers/*.rs` |
| MCP surface for agents (32 tools) | `src/mcp/handlers/`, `src/mcp/tools.rs` |
| Dispatch hook (auto-bind, lease, worktree creation) | `src/mcp/handlers/dispatch_hook/` |
| Fleet config (fleet.yaml parse, instances, teams) | `src/fleet.rs` |
| Team management | `src/teams.rs` |
| Task board | `src/tasks.rs` |
| Decisions store | `src/decisions.rs` |
| Schedules (cron + one-shot) | `src/schedules.rs` |
| Deployments (template-based multi-agent spawn) | `src/deployments.rs` |
| Worktree lifecycle (lease, release, GC) | `src/worktree_pool.rs`, `src/worktree.rs` |
| Worktree auto-cleanup (sweep merged/gone branches) | `src/worktree_cleanup.rs` |
| Binding state (agent ↔ branch ↔ worktree) | `src/binding.rs` |
| Inbox (message queue, delivery, supersede) | `src/inbox.rs` |
| Channel adapters (Telegram, Discord) | `src/channel/` |
| Daemon tick loop (CI watch, health, supervisor) | `src/daemon/` |
| CI watch (poll GitHub Actions, emit pass/fail) | `src/daemon/ci_watch.rs` |
| Claim verifier (push-time semantic gate) | `src/claim_verifier.rs` |
| TUI (multi-tab/pane terminal UI) | `src/tui.rs`, `src/render/`, `src/layout/` |
| App mode (in-process TUI + daemon) | `src/app/` |
| CLI argument parsing | `src/main.rs` (Commands enum) |
| Instructions/steering injection | `src/instructions.rs` |
| Admin (branch cleanup analysis) | `src/admin.rs` |
| Event log (audit trail) | `src/event_log.rs` |
| Health state | `src/health.rs` |
| Service (systemd/launchd integration) | `src/service/` |
| System tray | `src/tray/` |
| Bootstrap (agent resolve, doctor, fleet normalize) | `src/bootstrap/` |
| Git shim (worktree-deny matrix) | `src/bin/agend-git.rs` |
| MCP bridge (per-agent MCP server) | `src/bin/agend-mcp-bridge.rs` |

### Drift enforcement

`tests/no_dual_track_drift.rs` extracts top-level fn bodies from `src/agent_ops.rs` and `src/mcp/handlers.rs` and asserts that any fn sharing a name has an identical body, preventing the kind of silent divergence between MCP and daemon paths that caused the `cleanup_working_dir` Kiro-cleanup gap (14-entry MCP copy vs 19-entry canonical) on main before Task #9 Option C. The detector is parser-hardened against raw-string literals and `extern "ABI" fn` (PR #31).
