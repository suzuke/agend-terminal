[繁體中文](CLI.zh-TW.md)

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
Start the daemon using `fleet.yaml` or explicit `--agents`.

```
agend-terminal start [--foreground] [--fleet <path>]
agend-terminal start --agents <name:cmd>...        # ad-hoc, no fleet.yaml
```
- Detached service mode is the default. `--foreground` keeps stdio attached and blocks the calling shell, which is useful for debugging or an OS service manager.
- `--fleet <path>` — override fleet file. Default: `$AGEND_HOME/fleet.yaml`.
- `--agents <NAME:CMD>...` — start with explicit agent specs instead of `fleet.yaml`. Mutually exclusive with `--fleet` and implies `--foreground`. Subsumes the former `daemon` subcommand.

Example: `agend-terminal start --agents dev:claude reviewer:claude shell:/bin/bash`

On startup with fleet.yaml: prunes stale git worktrees, auto-creates a `general` instance if a Telegram channel is configured, initializes Telegram, and respawns any crashed agents per `HealthTracker`.

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

### `list` / `ls` / `status`
List running agents. Plain `list` queries the daemon's in-memory registry via `runtime::list_agents_with_fallback`; when the daemon API is briefly unresponsive (e.g. mid-restart) it falls back to scanning run-dir `.port` files so the command still returns a best-effort answer. Pass `--detailed`/`-d` (or `--json`, which implies detailed) for state / health / backend info via the daemon API (no fallback — `--detailed` requires the daemon to be reachable).

The daemon's in-memory registry is the canonical source of truth for "which agents exist"; the `.port` files are TUI-bridge per-agent socket artifacts and only surface in the offline fallback. Operator scripts wanting authoritative output should pipe `--json` rather than parse plain `list`.

```
agend-terminal list [--detailed] [--json] [--legacy-json]
agend-terminal ls   [--detailed] [--json] [--legacy-json]   # alias
agend-terminal status                                       # alias of `list` (kept for back-compat; use --detailed for state/health/cmd)
```

`status` is preserved as a clap alias for `list` post Wave 1 CLI consolidation; new code should prefer `list --detailed`.

#### JSON shape (#938)

`list --json` emits an envelope with a `mode` discriminator so operator scripts can distinguish authoritative output from offline-fallback output:

```json
{
  "mode": "live" | "fallback_daemon_stuck" | "fallback_daemon_absent",
  "agents": [ ... ]
}
```

- `live` — daemon API answered; `agents` is the rich registry response (`state` / `health` / `backend` fields populated).
- `fallback_daemon_stuck` — `.daemon` PID is alive but the API didn't respond (mid-restart, wedged main loop). `agents` carries `{name}`-only objects from the run-dir scan. May be transient; rerun before alerting. Persistent → `agend-terminal admin cleanup-zombies`.
- `fallback_daemon_absent` — no `.daemon` file or PID dead. Boot a daemon with `agend-terminal app` / `agend-terminal start`.

`--legacy-json` opts back into the pre-#938 shape (`{"agents": [...], ...}` passthrough of the API response, no `mode` field). One-release-cycle deprecation window for operator parsers that hard-code the old shape; remove after migration. Has no effect without `--json`.

Plain (non-JSON) `list` adds a one-line stderr hint when `mode != live` so an operator running the command interactively sees the fallback state without re-running with `--json`.

### `admin`

Operator-side housekeeping subcommands. Destructive paths prompt `[y/N]` unless `--yes` is supplied (intended for scripted recovery jobs).

```
agend-terminal admin cleanup-branches [--yes]
agend-terminal admin cleanup-zombies [--age <DURATION>] [--yes]
agend-terminal admin task-sweep-config [--repository <SLUG>] [--pause|--resume] [--dry-run|--no-dry-run] [--api-base-url <URL>]
agend-terminal admin gc-dry-run [--format human|json]
agend-terminal admin tokens [--action summary|by_instance] [--group-by instance|task] [--since <WINDOW>] [--instance <NAME>]
agend-terminal admin watchdog <snooze|resume|status|ack> [--duration <DURATION>]
agend-terminal admin config-set <KEY> <VALUE>
```

#### `admin cleanup-zombies` (#927)

Kill long-running zombie daemon processes that still hold a `<home>/run/<pid>/` directory. Lists every `.daemon` whose mtime is older than `--age` (default `14d`), prints the candidate set, then asks for confirmation before signaling.

- `--age <DURATION>` — accepts `14d`, `3h`, `30m` etc. Daemons younger than this are skipped.
- `--yes` — non-interactive; skips the `[y/N]` prompt and emits a "non-interactive destructive mode" audit log line.

Termination semantics are platform-asymmetric **by design** (#936 closed analysis):

- **Unix** — `SIGTERM` → 5 s grace → `SIGKILL`. The 5 s window covers the daemon's own `SHUTDOWN_GRACE=2s` agent teardown plus ~3 s for cleanup hooks and log-worker flush.
- **Windows** — `TerminateProcess` single-stage. The Win32 surface this CLI uses today has no SIGTERM equivalent. A future improvement may add a `CTRL_BREAK_EVENT` path for two-stage parity.

Exit codes:

- `0` — all candidates reaped (or none found).
- non-zero — at least one process refused to die within the grace window (kernel-stuck / uninterruptible sleep / kernel module hold). Operator must investigate manually.

`agend-terminal list` surfaces a `cleanup-zombies` hint in its fallback message when it detects a stuck daemon. The hint is intentionally cautious — the fallback can also fire transiently mid-restart, so wait one cycle before invoking `cleanup-zombies`.

#### `admin cleanup-branches`

Delete local branches whose PRs have been merged (squash-merge safe). Default is dry-run (preview only); `--yes` actually deletes. See `docs/RCA-*` notes for the squash-merge detection heuristic.

#### `admin task-sweep-config` (#2547)

View or configure the GitHub-PR auto-close sweep daemon (polls merged PRs and emits `Done` events for `Closes t-XXX-N` markers). Moved here from the `task_sweep_config` MCP tool — operator-only setting, zero agent calls in 20 days. With no flags, prints the current config unchanged.

- `--repository <owner/repo>` — GitHub slug to sweep. Empty string disables.
- `--pause` / `--resume` — pause/resume the sweep tick (mutually exclusive).
- `--dry-run` / `--no-dry-run` — log decisions without emitting events, or emit for real (mutually exclusive).
- `--api-base-url <URL>` — REST API base for self-hosted GitHub Enterprise. Empty string resets to `https://api.github.com`.

#### `admin gc-dry-run` (#2548)

List Phase 4 GC candidates (released, past-grace, daemon-managed worktrees) without deleting them. Non-destructive. Moved from the `gc_dry_run` MCP tool (zero calls in 20 days).

- `--format human|json` — output format (default `human`).

#### `admin tokens` (#2548)

On-demand token usage + estimated USD cost from Claude Code / Codex session transcripts. Moved from the `tokens` MCP tool (zero calls in 20 days). Cost is an estimate; OpenCode/Kiro/Gemini are not covered.

- `--action summary|by_instance` — `summary` (default) is fleet totals + per-instance table; `by_instance` requires `--instance`.
- `--group-by instance|task` — `instance` (default) is per-instance/per-model; `task` time-joins each message to the active task.
- `--since <WINDOW>` — lookback window, e.g. `24h` (default), `7d`, `90m`, `all`.
- `--instance <NAME>` — required for `--action by_instance`; optional filter for `summary`.

#### `admin watchdog` (#2548)

Fleet idle watchdog control. Moved from the `watchdog` MCP tool (zero calls in 20 days). `ack` suppresses fleet alerts until post-ack agent activity is detected, then auto-clears.

- `<snooze|resume|status|ack>` — positional action.
- `--duration <DURATION>` — snooze duration, e.g. `2h`, `30m`, `1h30m`. Clamped to max 4h. Default `1h`.

#### `admin config-set` (#2548)

Set a runtime-mutable daemon config key. Moved from the `config` MCP tool's `set` action (zero MCP calls in 20 days) — agents can still read config via the `config` MCP tool (`get`/`list`), but only the operator can mutate it now. See the `config` tool's live description (`docs/MCP-TOOLS.md`) for the current key list.

- `<KEY> <VALUE>` — positional key and new value.

### `connect`
Run an externally requested backend under daemon registration. `connect` registers the instance, spawns the backend command, waits for it to exit, then deregisters it; it is not an attach operation for an already-running process.

```
agend-terminal connect <name> --backend <backend> [--working-dir <dir>] [-- <extra-args>...]
```
- `--backend` — `claude`, `kiro-cli`, `codex`, `opencode`, `antigravity-cli` (binary `agy`), or `grok`. The aliases `agy` / `antigravity` / `antigravity-cli` resolve to the same backend. Gemini CLI is retired.
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

### `agend-mcp-bridge` (separate binary)

Start the MCP stdio server for the current instance. Intended to be invoked by an agent's backend, not by humans directly — the relevant backend config is auto-written to the agent's working directory by `mcp_config.rs`. The bridge proxies all tool calls to the running daemon's TCP API; no local-handler fallback exists.

```
AGEND_INSTANCE_NAME=<name> agend-mcp-bridge
```

Running without `AGEND_INSTANCE_NAME` is allowed but enters standalone mode and emits a warning. Sprint 56 Track I (#531) retired the previous `agend-terminal mcp` subcommand; see the [Phase 1 RCA](archived/RCA-issue-531-deprecate-agend-terminal-mcp-2026-05-08.md) for the migration history.

### `capture`
Capture backend output or promote a passive capture into the state-replay corpus.

```
agend-terminal capture backend --backend <name> [--seconds <N>]    # default 15s
agend-terminal capture promote <capture.cap> <scenario> --scenario-kind <kind>
```

### `verify`
Full end-to-end verification across backends (spawns each configured backend, verifies PTY + VTerm + MCP wiring).

```
agend-terminal verify [--json] [--backend <name>] [--quick]
```

- `--quick` — skip per-backend tests + daemon-spawning tests; runs only the 4 in-process probes (attach, inbox, mcp framing, api). Completes in <30s. Subsumes the former `test` subcommand.

### `doctor`
Health check: home directory, `.env`, `fleet.yaml` parse, active sockets, backend binary presence + version (plus a note if the installed backend version differs from the calibrated one used for state detection).

```
agend-terminal doctor
```

### `quickstart`
Interactive setup wizard: detects installed backends, optionally configures Telegram, writes `fleet.yaml` + `.env`. Handles existing config without stomping it.

```
agend-terminal quickstart [--unattended]
```

`--unattended` never reads stdin or waits on the network; it is intended for CI and scripted installs.

### `mode`
Set operator availability and optional sleep-mode delegation. Operator-only authority control.

```
agend-terminal mode <active|away|sleep> [--delegate <instance>] [--scope <op,...>]
```

### `service`
Install, remove, or inspect the user-level OS service:

```
agend-terminal service <install|uninstall|status>
```

### `skills`
Manage the unified skills source and install it into backend-specific paths:

```
agend-terminal skills <add|remove|list|update|install> ...
```

### `verify-push`
Verify a semantic push claim against the actual diff:

```
agend-terminal verify-push --base <commit> [--head <commit>] (--claim <text>|--claim-from-stdin) [--json]
```

### `bugreport`
Generate a single text file with diagnostics, recent logs, and redacted config. Writes under `AGEND_HOME/bugreports/`.

```
agend-terminal bugreport
```

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
| `AGEND_LOG` | `tracing-subscriber` env filter | `agend_terminal=info` (see precedence note below) |
| `AGEND_LOG_RETAIN_DAYS` | Daily rotation retain count (#914) | `3` |
| `AGEND_LOG_MAX_BYTES` | Hard directory-size backstop (#914); supports `K`/`M`/`G` suffix | `2G` |
| `AGEND_INSTANCE_NAME` | Identifies the instance to the MCP server | *(set by spawner)* |
| `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` | Boot-time stale-`run/<pid>/` GC, ages older than N days (#933). `0` / unset disables. Destructive — use with care. | *(disabled)* |
| `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN` | When `1`, the boot sweep logs the would-delete set instead of unlinking (#933). Pairs with `AGE_DAYS` for safe trials before enabling destructive mode. | *(disabled)* |
| `AGEND_DAEMON_THREAD_DUMP_SECS` | Periodic in-process thread state dump, every N seconds (#941). `0` / unset disables; any positive integer enables. Output appears in `daemon.log`. Zero overhead when unset. | *(disabled)* |
| Telegram env | `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID` | *(optional; read from `.env` under `$AGEND_HOME`)* |

**`AGEND_LOG` precedence (#927 PR-A)** — when the env var is set, it wins over the in-code default (`agend_terminal=info`). The default only applies when the var is unset or empty. This was previously documented as "default" but implementation occasionally overrode caller-set env values; the precedence is now explicit and tested.

**Destructive env-var safety** — `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` deletes `run/<pid>/` directories outright (no archive). Before flipping it on, run with `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN=1` and `grep "boot-sweep" $AGEND_HOME/daemon.log` to validate the candidate set against expectations.

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
        .daemon                   # pid:start_time — identity for liveness checks (early)
        api.cookie                # 32-byte auth cookie for api.port (0600 on Unix)
        api.port                  # daemon control API TCP port (loopback)
        .ready                    # boot-completion sentinel (#922); daemon-init-complete signal
        <agent>.port              # per-agent TUI bridge TCP port (loopback, cookie-auth)
```

`.ready` exists ⟹ the daemon's agent spawn loop has finished and `list` / `/api/list` returns the final agent set for this boot. Single-signal policy — future sub-stage readiness MUST extend `.ready`'s content rather than introduce a new file. See `CLAUDE.md` "Daemon lifecycle files (#922)" for the full table and bare-poll caveats (residual `.ready` from a crashed daemon needs to be combined with a PID-liveness check; `agend-terminal doctor` is the recommended idiom).

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
