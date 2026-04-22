# CLI Reference

All commands are dispatched via `clap` from `src/main.rs` (enum `Commands`). Run `agend-terminal --help` for terse help or `<cmd> --help` per subcommand.

Data root is controlled by `AGEND_HOME` (defaults to `~/.agend`, falls back to `~/.agend-terminal` for backwards compat). Logs honor `AGEND_LOG` (e.g. `AGEND_LOG=agend_terminal=debug`).

## Running without arguments

```
agend-terminal
```

Prints `--help` and exits. For the interactive multi-pane TUI, use `agend-terminal app`.

## Commands

### `app`
Launch the multi-tab/pane TUI with in-process agent management. This is the primary user-facing entry point after `0.3.0`.

```
agend-terminal app [--fleet <path>]
```
- `--fleet <path>` — override fleet file. Default: `$AGEND_HOME/fleet.yaml`.

Keybinds: see `src/keybinds.rs`. Prefix `Ctrl+B`, then `c` new tab, `n`/`p` next/prev, `"` / `%` split, `o` focus next pane, `x` close, `z` zoom, `[` scroll mode, `:` command palette, `d` detach, `?` help. Uppercase `D` / `T` open decisions / task overlays. `Space` cycles layout presets.

### `start`
Start the daemon using `fleet.yaml`.

```
agend-terminal start [--detached] [--fleet <path>]
```
- `--detached` — background the daemon (stdio → `$AGEND_HOME/daemon.log`); the foreground process exits once the daemon has published its run dir.
- `--fleet <path>` — override fleet file. Default: `$AGEND_HOME/fleet.yaml`.

On startup: prunes stale git worktrees, auto-creates a `general` instance if a Telegram channel is configured, initializes Telegram, and respawns any crashed agents per `HealthTracker`.

### `daemon`
Start the daemon with explicit agent specs (no `fleet.yaml`).

```
agend-terminal daemon [agents...]
# agents are "name:command" pairs
agend-terminal daemon dev:claude reviewer:claude shell:/bin/bash
```

### `attach`
Attach to a single agent's PTY (terminal view). `Ctrl+B d` to detach, daemon keeps the agent running.

```
agend-terminal attach [<name>]      # default: shell
```

### `inject`
Write arbitrary text to an agent's PTY (append `\r` if you need a newline).

```
agend-terminal inject <name> <text...>
```

### `list` / `ls`
List running agents.

```
agend-terminal list [--json]
agend-terminal ls   [--json]
```

### `status`
Detailed per-agent status: state (Idle, Thinking, ToolUse, RateLimit, Crashed, …), health counters, restart history.

```
agend-terminal status [--json]
```

### `connect`
Register an *already-running* local agent with the daemon (inbox-only — no PTY management). Useful in headless environments or to mix a manually-launched CLI into a running fleet.

```
agend-terminal connect <name> --backend <backend> [--working-dir <dir>] [-- <extra-args>...]
```
- `--backend` — `claude`, `kiro-cli`, `codex`, `opencode`, `gemini`.
- `--working-dir` — defaults to current directory.
- Extra args after `--` are passed to the backend.

### `kill`
Stop a specific agent. Daemon keeps running.

```
agend-terminal kill <name>
```

### `stop`
Stop the daemon (also terminates all managed agents).

```
agend-terminal stop
```

### `fleet`
Fleet management subcommands.

```
agend-terminal fleet start [<config>]   # alias for top-level `start`, optional fleet path
agend-terminal fleet stop               # alias for top-level `stop`
```

### `mcp`
Start the MCP stdio server for the current instance. Intended to be invoked by an agent's backend, not by humans directly — the relevant backend config is auto-written to the agent's working directory by `mcp_config.rs`.

```
AGEND_INSTANCE_NAME=<name> agend-terminal mcp
```

Running without `AGEND_INSTANCE_NAME` is allowed but enters standalone mode and emits a warning.

### `capture`
Spawn a backend CLI for N seconds and dump its VTerm screen (ANSI-stripped). Used for debugging state-detection regexes and onboarding new backends.

```
agend-terminal capture --backend <name> [--seconds <N>]    # default 15s
```

### `test`
Internal QA hooks. Available suites: `mcp` (frame format sanity), `attach` (PTY spawn + inject), `inbox` (enqueue/drain), `api` (daemon API probe), `all` (attach + inbox).

```
agend-terminal test [<suite>]     # default: all
```

### `verify`
Full end-to-end verification across backends (spawns each configured backend, verifies PTY + VTerm + MCP wiring).

```
agend-terminal verify [--json] [--backend <name>]
```

### `doctor`
Health check: home directory, `.env`, `fleet.yaml` parse, active sockets, backend binary presence + version (plus a note if the installed backend version differs from the calibrated one used for state detection).

```
agend-terminal doctor
```

### `demo`
Interactive 30-second demo — spawns two fake agents (`alice`, `bob`), scripts a short conversation with split-screen rendering, and demonstrates crash recovery. No real AI backend required.

```
agend-terminal demo
```

### `quickstart`
Interactive setup wizard: detects installed backends, optionally configures Telegram, writes `fleet.yaml` + `.env`. Handles existing config without stomping it.

```
agend-terminal quickstart
```

### `bugreport`
Generate a single text file with diagnostics, recent logs, and redacted config. Drops to current directory.

```
agend-terminal bugreport
```

### `upgrade` (Unix only)
Hot-upgrade the daemon to a new binary via `agend-supervisor`. Flow: stage new binary → self-test → stop old daemon → start new → wait for ready ping → stabilise for N seconds → commit (or roll back).

```
agend-terminal upgrade --binary <path> [--to-version <label>] [--yes] \
                       [--install-supervisor] \
                       [--stability-secs <N>] [--ready-timeout-secs <N>]
```
- `--binary <path>` — path to the new daemon binary (required).
- `--to-version <label>` — human-visible version label; defaults to the new binary's `--version` output.
- `--yes` — skip interactive confirmation. Required with `--install-supervisor`.
- `--install-supervisor` — idempotent bootstrap of the supervisor layout on first upgrade.
- `--stability-secs <N>` — stability window after switchover, seconds. Default `60`, `0` disables.
- `--ready-timeout-secs <N>` — ready-ping timeout, seconds. Default `60`, `0` disables.

See `docs/architecture.md` Module 8 for the supervisor design; Windows is not supported (the socket-swap + symlink-rename trick is Unix-only).

### `completions`
Print shell completion scripts to stdout.

```
agend-terminal completions <shell>
# shell ∈ bash | zsh | fish | elvish | powershell
```

---

## Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `AGEND_HOME` | Data / config root | `~/.agend` (fallback: `~/.agend-terminal`) |
| `AGEND_LOG` | `tracing-subscriber` env filter | `agend_terminal=info` |
| `AGEND_INSTANCE_NAME` | Identifies the instance to the MCP server | *(set by spawner)* |
| Telegram env | `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID` | *(optional; read from `.env` under `$AGEND_HOME`)* |

## On-disk Layout

```
$AGEND_HOME/
    .env                          # optional; key=value, supports `export` prefix and quoted values
    fleet.yaml                    # agent definitions
    decisions/                    # decision JSON files
    tasks/                        # task board state
    inbox/<agent>.jsonl           # per-agent message queue
    metadata/                     # miscellaneous state
    downloads/                    # Telegram attachment downloads
    snapshot.json                 # fleet snapshot
    event-log.jsonl               # event log
    workspace/<agent>/            # default working dir when none set
    run/<daemon-pid>/
        .daemon                   # pid:start_time
        api.port                  # daemon control API TCP port (loopback)
        api.cookie                # 32-byte auth cookie for api.port (0600 on Unix)
        <agent>.port              # per-agent TUI bridge TCP port (loopback, cookie-auth)
```

Everything under `$AGEND_HOME` (including `fleet.yaml`, `session.json`) is locked via `fs2::FileExt` during mutations — safe against concurrent daemon / CLI usage.

## Exit Codes

- `0` — success.
- `1` — invalid input or command failed.
- Other non-zero codes come from the child process in commands like `inject` / `attach`.

## See Also

- `docs/MCP-TOOLS.md` — MCP tools exposed to each agent.
- `docs/architecture.md` — daemon design and module map.
- `CHANGELOG.md` — version history.
- `CONTRIBUTING.md` — how to develop and test.
