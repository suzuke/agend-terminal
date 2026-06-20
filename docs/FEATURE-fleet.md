[繁體中文](FEATURE-fleet.zh-TW.md)

# Fleet Management — Unified Agent Configuration

## Usage Scenarios

> **Target audience:** Operators — used through CLI or TUI.

**Defining your agent team.** You need a lead for task decomposition, a dev for implementation, and a reviewer for code review — all working on the same repo but in isolated worktrees. fleet.yaml lets you declare all three in one file, specifying each agent's role, backend, and working directory.

**Managing shared configuration.** All your agents need the same environment variables and ready patterns, but one uses a different backend. The `defaults` section handles the shared config while individual instances override what's different.

**Scaling up.** Your project grows and you need a second dev or a dedicated tester. Add a few lines to fleet.yaml, restart the daemon, and the new agent is running with the right configuration — including team membership, worktree, and Telegram topic.

## Motivation

Before fleet.yaml, launching multiple AI agents meant opening separate terminals, configuring environment variables, and specifying working directories for each one. Agents couldn't collaborate, and there was no unified lifecycle management.

fleet.yaml solves this: a single YAML file describes every agent's configuration — which backend to use, where to work, which team to belong to, and what communication channel to use. `agend-terminal start` reads fleet.yaml and automatically launches all agents; the daemon handles health monitoring, auto-restart, and cross-agent communication.

---

## fleet.yaml Structure

fleet.yaml lives at `$AGEND_HOME/fleet.yaml` (default `~/.agend-terminal/fleet.yaml`).

### Full Example

```yaml
# Default configuration (inherited by all instances)
defaults:
  backend: claude
  ready_pattern: "bypass permissions|❯"
  env:
    AGEND_PRODUCTIVE_GATE: "1"

# Communication channel
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456789
  mode: topic
  user_allowlist:
    - 12345

# Display timezone (IANA format)
display_timezone: Asia/Taipei

# Agent instances
instances:
  lead:
    role: "Team lead — task decomposition and dispatch"
    backend: claude
    model: opus
    working_directory: ~/Projects/my-app
    source_repo: ~/Projects/my-app
    worktree: false

  dev:
    role: "Primary developer"
    working_directory: ~/Projects/my-app
    source_repo: ~/Projects/my-app
    github_login: my-github-user

  reviewer:
    role: "Code reviewer"
    role_kind: reviewer   # typed role → trims the MCP tool surface (opt-in; see below)
    backend: kiro-cli
    working_directory: ~/Projects/my-app
    source_repo: ~/Projects/my-app

# Teams
teams:
  core:
    members: [lead, dev, reviewer]
    orchestrator: lead
    description: "Core development team"
    source_repo: ~/Projects/my-app
```

### Section Reference

#### `defaults` — Default Configuration

All instances inherit settings from defaults. Individual instances can override any field.

| Field | Type | Description |
|-------|------|-------------|
| `backend` | string | Backend name (claude / kiro-cli / codex / opencode / gemini / agy / shell) |
| `command` | string | Custom command (overrides backend default) |
| `args` | [string] | CLI argument list |
| `model` | string | Model name (e.g., opus, sonnet) |
| `ready_pattern` | string | Regex to determine when the agent is ready |
| `env` | map | Environment variables (key-value pairs) |
| `cols` | int | Terminal width (default 200) |
| `rows` | int | Terminal height (default 50) |

#### `instances` — Agent Instances

Each key is the agent's name (must match `[a-zA-Z0-9_-]`); the value is its configuration.

| Field | Type | Description |
|-------|------|-------------|
| `role` | string | Agent role description, free text (alias: `description`) |
| `role_kind` | enum | **Optional, opt-in.** Typed role selector that trims the MCP tool surface this agent advertises (defense-in-depth, #2300). One of `reviewer` / `planner` / `explorer` (read/report — drops instance/worktree lifecycle + orchestration tools) or `orchestrator` / `implementer` / `utility` / `proxy` (full surface). **Absent → all tools** (no existing fleet changes). Distinct from the free-text `role` above. Strict + fail-closed: an unknown value fails fleet load, and a malformed fleet never silently widens the advertised tool surface (#2367). |
| `backend` | string | Override defaults backend |
| `command` | string | Override defaults command |
| `args` | [string] | Additional CLI arguments (merged with defaults) |
| `working_directory` | string | Working directory (supports `~/` expansion). Defaults to `$AGEND_HOME/workspace/<name>/` if unset |
| `source_repo` | string | Git repository path for automatic worktree creation. Separate from `working_directory` so worktrees can live elsewhere |
| `repo` | string | GitHub `owner/repo` format. Used for CI watch, PR operations, etc. Auto-derived from `source_repo` git remote; this field is a manual override |
| `worktree` | bool | `true` (default) = auto-create git worktree; `false` = skip |
| `git_branch` | string | Custom worktree branch name (alias: `worktree_source`) |
| `model` | string | Model override |
| `env` | map | Environment variables (merged with defaults; instance takes precedence) |
| `cols` / `rows` | int | Terminal size override |
| `ready_pattern` | string | Readiness regex override |
| `display_name` | string | Display name in UI and Telegram |
| `instructions` | string | Path to additional instructions file (relative to fleet.yaml directory) |
| `github_login` | string | GitHub username for task sweep author verification |
| `skills` | [string] | Allowlist of skills this agent can use |
| `topic_id` | int | Telegram topic ID (auto-managed by daemon; usually not set manually) |
| `topic_binding_mode` | string | Topic creation mode: `auto` (default) / `skip` / `deferred` |

#### `channel` — Communication Channel

Two channel types are currently supported: Telegram and Discord.

**Telegram:**

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN    # Environment variable name (not the token itself)
  group_id: -100123456789           # Supergroup ID
  mode: topic                       # topic (forum mode)
  user_allowlist: [12345, 67890]    # Allowed Telegram user IDs
  fleet_binding:                    # Optional: agent-topic binding
    dev: 42
    reviewer: 43
```

**Discord:**

```yaml
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_TOKEN
  guild_id: "123456789"
```

`user_allowlist` is a security mechanism — Telegram users not on the list cannot send commands to agents via the bot. This field is required.

#### `teams` — Teams

Group multiple agents into teams to enable cross-agent collaboration (task dispatch, code review, etc.).

| Field | Type | Description |
|-------|------|-------------|
| `members` | [string] | Instance names of team members |
| `orchestrator` | string | Team coordinator (receives task assignments and progress reports) |
| `description` | string | Team description |
| `source_repo` | string | Shared git repository path |

#### `display_timezone` — Display Timezone

Sets the timezone the daemon uses in human-readable timestamps. Accepts IANA timezone names (e.g., `Asia/Taipei`, `America/New_York`). Falls back to system timezone if unset.

#### `templates` — Deployment Templates

Defines reusable agent configuration templates for dynamically creating instances via `fleet deployment deploy`.

#### `watchdog` — Watchdog Topology

Controls which agent the idle watchdog watches and who receives each watchdog /
anti-stall / decision-timeout notification. These are agent / recipient **names**
(fleet topology), so they live here rather than in env vars. Every field is
optional; an omitted block (or field) falls back to the legacy `AGEND_*` env var
(deprecated) and then to a built-in default. Resolution precedence:

**`watchdog:` value > `AGEND_*` env var (deprecated, warns once) > built-in default.**

```yaml
watchdog:
  # Legacy SINGLE-AGENT mode for the dev-vantage idle watchdog. When set, the
  # watchdog watches ONLY this agent (with the global dev_idle_threshold_secs)
  # instead of iterating every fleet instance. Omit it (default) to keep the
  # modern per-instance iteration. Mirrors AGEND_IDLE_WATCHDOG_AGENT.
  idle_watchdog_agent: dev
  # Recipient for dev-vantage idle alerts. Default: lead.
  dev_recipient: lead
  # Recipient for fleet-vantage idle alerts ("the whole fleet is quiet").
  # Default: lead (#1563: was general, which over-pinged the general assistant).
  fleet_recipient: lead
  # Recipients for task-stall warnings. Default: [general, lead].
  task_stall_recipients:
    - general
    - lead
  # Recipient for the decision-timeout auto-default (operator-proceed) emission.
  # Default: general.
  decision_timeout_recipient: general
```

The matching env vars (`AGEND_IDLE_WATCHDOG_AGENT`, `AGEND_IDLE_WATCHDOG_DEV_RECIPIENT`,
`AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT`, `AGEND_TASK_STALL_RECIPIENTS`,
`AGEND_DECISION_TIMEOUT_RECIPIENT`) are **deprecated** — they still work for one
window but will be removed; see `docs/env-vars.md` §8.

### Environment Variables

#### Merge Priority

Environment variables are resolved in this order (highest priority wins):

1. **Runtime SPAWN params** — `env` object passed in the MCP `start_instance` / SPAWN API call
2. **Instance `env`** — `instances.<name>.env` in fleet.yaml
3. **Defaults `env`** — `defaults.env` in fleet.yaml
4. **Daemon defaults** — automatically injected by the daemon (see below)

Within fleet.yaml, `defaults.env` is applied first, then `instances.<name>.env` extends/overrides it. If the runtime SPAWN call includes an `env` object, it replaces the fleet.yaml-resolved env entirely.

#### Daemon-Injected Variables

The daemon injects these variables into every agent process before applying fleet.yaml env:

| Variable | Value | Purpose |
|----------|-------|---------|
| `AGEND_INSTANCE_NAME` | Agent name | Identifies the agent to MCP tools and daemon IPC |
| `TERM` | `xterm-256color` | Terminal type for PTY rendering |
| `COLORTERM` | `truecolor` | Enables 24-bit color support |
| `FORCE_COLOR` | `1` | Forces colored output in tools that check this |
| `GIT_EDITOR` | `true` | Prevents git from opening an interactive editor |
| `GIT_SEQUENCE_EDITOR` | `true` | Prevents `git rebase -i` from opening an editor |
| `EDITOR` | `true` | Fallback editor suppression |
| `VISUAL` | `true` | Fallback editor suppression |
| `LANG` | `en_US.UTF-8` | Set only if `LANG` is not already in the environment |

The `GIT_EDITOR` family prevents agents from getting stuck in an interactive editor (e.g., Vim) during git operations. Fleet.yaml env entries override these defaults — setting `instances.dev.env.GIT_EDITOR: vim` restores interactive editing for that agent.

#### Sensitive Key Deny-List

The following environment variable names are **blocked** from fleet.yaml `env` to prevent accidental credential exposure or runtime hijacking. Attempts to set these are silently dropped with a warning log:

| Category | Blocked Keys |
|----------|-------------|
| AI backend credentials | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, `GEMINI_API_KEY` |
| Cloud credentials | `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN` |
| Git forge tokens | `GITHUB_TOKEN`, `GITLAB_TOKEN`, `NPM_TOKEN` |
| Dynamic linker injection | `LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `DYLD_INSERT_LIBRARIES`, `DYLD_LIBRARY_PATH`, `DYLD_FALLBACK_LIBRARY_PATH` |
| AgEnD runtime internals | `AGEND_HOME`, `AGEND_INSTANCE_NAME` |

These credentials should be set in the host shell environment or `.env` file instead — the daemon process inherits them and passes them through to agent subprocesses automatically.

#### Examples

**Proxy configuration:**

```yaml
defaults:
  env:
    HTTP_PROXY: "http://proxy.corp:8080"
    HTTPS_PROXY: "http://proxy.corp:8080"
    NO_PROXY: "localhost,127.0.0.1"
```

**API keys via host environment (recommended):**

```yaml
# DON'T put secrets in fleet.yaml:
#   env:
#     MY_API_KEY: "sk-abc123"    # BAD — checked into git
#
# Instead, set in your shell profile or .env:
#   export MY_SERVICE_API_KEY=sk-abc123
#
# Then reference in fleet.yaml only if you need to rename:
defaults:
  env:
    SERVICE_KEY_ALIAS: "${MY_SERVICE_API_KEY}"
```

**Per-instance overrides:**

```yaml
defaults:
  env:
    LOG_LEVEL: info

instances:
  dev:
    env:
      LOG_LEVEL: debug        # overrides defaults
      RUST_BACKTRACE: "1"     # added for this instance only
  reviewer:
    role: "Code reviewer"
    # inherits LOG_LEVEL=info from defaults
```

---

## Startup Process

### `agend-terminal start`

```
agend-terminal start
```

The startup sequence proceeds as follows:

1. **Daemon lock**: Acquires an exclusive lock on `$AGEND_HOME/.daemon.lock`, ensuring only one daemon runs at a time. If another daemon is already running, it suggests using `attach` or `app` to connect.

2. **Cleanup residuals**: Scans and cleans up run directories and zombie processes left from previous abnormal exits.

3. **Load fleet.yaml**: Reads and parses the YAML, then normalizes:
   - If fleet.yaml is empty, automatically creates a `general` instance
   - Auto-assigns UUIDv4 to instances missing an `id` field
   - Normalizes `channels` (plural) to `channel` (singular)

4. **Pre-flight checks**: Runs doctor validation (confirms backend executables exist, ports are available, etc.).

5. **Resolve agents**: For each instance:
   - Merges defaults and instance configuration
   - Expands `~/` paths
   - Validates backend and ready_pattern
   - Creates working directory (if it doesn't exist)
   - Creates git worktree (if `source_repo` or `git_branch` is set and `worktree` is not `false`)

6. **Initialize Telegram**: If a channel is configured, establishes the bot connection and creates or binds Telegram topics for each agent.

7. **Set up git shim**: Injects the `agend-git` wrapper into `$PATH`, allowing the daemon to intercept and manage agent git operations.

8. **Launch all agents**: Sequentially spawns each agent's PTY process:
   - Constructs the command line (backend preset + user args + environment variables)
   - Opens a PTY (pseudo-terminal)
   - Starts the subprocess
   - Registers with the agent registry
   - Starts the PTY reader thread
   - Brief stagger delay between agents to avoid simultaneous launch overhead

9. **Write ready marker**: Writes a `.ready` file once daemon initialization is complete.

### Foreground Mode

```
agend-terminal start --foreground
```

By default, `start` runs as a detached service (background). Adding `--foreground` keeps it in the foreground with stdout/stderr going directly to the terminal — useful for debugging or running under a process supervisor (systemd / launchd).

### Direct Agent Specification

```
agend-terminal start --agents dev:claude reviewer:kiro-cli
```

Skips fleet.yaml and directly specifies agents in `name:backend` format. This mode implies `--foreground`.

---

## Resume Mode

When the daemon restarts (auto-restart after crash or manual stop/start), agents can resume their previous conversation state instead of starting fresh.

### Resume Behavior by Backend

| Backend | Resume Flag | Description |
|---------|------------|-------------|
| Claude Code | `--continue` | Resumes the most recent conversation in the working directory |
| Kiro CLI | `--resume` | Resumes the most recent conversation |
| Codex | Built-in | Session managed internally by Codex |
| OpenCode | `--continue` | Resumes the most recent conversation |
| Gemini | `--resume latest` | Resumes the most recent conversation |
| Agy | `--continue` | Resumes the most recent conversation |
| Shell | Not supported | Every launch is a new session |

### Fallback Mechanism

If the daemon tries to start an agent in resume mode but detects no recoverable session (e.g., first launch or session files have been cleared), it automatically falls back to fresh mode, preventing `--continue` from erroring on an empty session.

---

## Lifecycle Management

### Stopping the Daemon

```
agend-terminal stop
```

Gracefully stops the daemon and all agents.

### Status Queries

```
agend-terminal list              # Simple list (agent names)
agend-terminal list --detailed   # Detailed info (state, health, backend)
agend-terminal list --json       # JSON output
```

### Health Monitoring

The daemon continuously monitors each agent's health:

- **Healthy**: Running normally
- **Recovering**: Recovering after a crash
- **Unstable**: Multiple crashes in a short window
- **Failed**: Exceeded max retry count; auto-restart disabled
- **Hung**: Agent unresponsive (pending input with no response past timeout)
- **IdleLong**: Extended inactivity (no pending input; not abnormal)

The auto-restart mechanism uses exponential backoff starting at 5 seconds, capped at 5 minutes, tracking crash count within a 10-minute window.

---

## fleet.yaml Field Merge Rules

When fleet.yaml is updated (e.g., via `fleet deployment deploy` or manual editing), fields fall into two categories:

### Daemon-Managed Fields

The following fields are automatically managed by the daemon; daemon values take precedence during merges:

- `id`: Instance UUID
- `topic_id`: Telegram topic ID
- `git_branch`: Current worktree branch
- `source_repo`: Git repository path

### Operator-Controlled Fields

All other fields (`role`, `backend`, `env`, `working_directory`, etc.) are operator-controlled. If a conflict is detected during merge, the daemon reports an error rather than silently overwriting.

---

## FAQ

### Q: Do I need to restart the daemon after modifying fleet.yaml?

Yes. Currently, fleet.yaml changes require `stop` + `start` to take effect.

### Q: Can an agent belong to multiple teams?

The `teams` structure in fleet.yaml doesn't prevent this, but the MCP communication tools' team routing assumes each agent belongs to at most one team.

### Q: How do I add a new agent?

Three ways, depending on your workflow:

**1. Edit fleet.yaml** — add a new entry under `instances`, then restart the daemon:

```yaml
instances:
  # ...existing agents...
  new-agent:
    role: "New agent for feature X"
    working_directory: ~/Projects/feature-x
```

**2. TUI app mode** — in `agend-terminal app`, use the built-in UI to create a new instance interactively without editing YAML.

**3. MCP tool** — any running agent can programmatically create a new instance via the `create_instance` MCP tool, useful for automated orchestration and deployment templates.