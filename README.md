# AgEnD Terminal

Orchestrate AI coding agents — not just run them.

> ⚠️ **Pre-alpha.** APIs, CLI flags, and `fleet.yaml` schema may change
> between minor versions. Not for production use. Pin a specific version
> and read the release notes before upgrading.

```bash
cargo install agend-terminal
agend-terminal demo    # Try it in 30 seconds
```

## ⚠️ Git Behavior Modification (Important)

agend-terminal does **not** run AI agents against vanilla `git`. To coordinate multiple agents safely on the same repo, the daemon installs a thin shim layer between agents and your real `git` binary. **Read this section before installing** — once you start the daemon, the modifications below take effect.

### What gets modified

- **PATH shim for agent processes.** A symlink at `$AGEND_HOME/bin/git` points to a small Rust binary (`agend-git`). When the daemon spawns an agent's PTY, that path is prepended to the agent's `PATH`. Agents that invoke `git` end up running the shim; the shim forwards almost every command to your real `git` (resolved via `AGEND_REAL_GIT` or `which`).
- **Per-worktree commit hooks.** For agent-managed worktrees, the daemon points `core.hooksPath` to `$AGEND_HOME/hooks` and installs a `prepare-commit-msg` hook that appends `Agend-Agent`, `Agend-Branch`, `Agend-Issued-At`, and (when present) `Agend-Task` trailers to the commit message. Trailers are skipped if already present (idempotent).
- **Deny matrix on agent git ops.** The shim refuses certain commands from unbound or cross-branch contexts: `git worktree add/remove/move`, `git checkout` of a different branch, etc. The daemon owns the worktree pool — see Phase 3 lease in `docs/proposals/agend-git-shim.md`.
- **Auto bind/lease on dispatch (or via `bind_self`).** When you delegate a task to an agent with a `branch` field, the daemon auto-creates a managed worktree, marks it with a `.agend-managed` file, and writes a `binding.json` recording the agent → branch link.
- **Worktree lifecycle is daemon-managed.** Cleanup is via the `release_worktree` MCP tool, not direct `git worktree remove`. A daemon-side hourly GC sweep flags stale entries (currently dry-run only; not auto-deleted).

### Why

- **Multi-agent safety.** Multiple AI agents working in the same repo without isolation will race on the same branch. Per-agent worktrees make that impossible at the git layer rather than relying on agent-side discipline.
- **Audit trail.** `Agend-Agent: <name>` trailer answers "which agent made this commit?" without parsing chat logs. Useful when reviewing autonomous work, much more useful when something went wrong.
- **Lifecycle hygiene.** Crashed agents, stale dispatches, and abandoned branches accumulate fast in a multi-agent setup. The daemon's bind/lease/release gives the cleanup work a single owner.
- **Safety guard rails.** The deny matrix catches the obvious foot-guns (agents accidentally checking out `main`, deleting other agents' worktrees) at the shim layer instead of after the fact.

### Risk

- **Agents see a different `git` than you do.** The PATH injection only happens inside agent PTYs spawned by the daemon. Your own terminal's `git` is unchanged. But if you compare what an agent did against `git log` from your shell, the agent's command went through the shim and the shim may have intercepted it. Set `AGEND_GIT_BYPASS=1` if you need to reproduce an agent's exact bare-`git` behavior.
- **Commits gain extra trailers.** Tools that parse commit messages strictly (some changelog generators, some CLA bots) may need their parsers updated. Standard `git log --format` output is unaffected; the trailers are appended after the commit body.
- **Some commands deny unexpectedly.** A new agent or operator unfamiliar with the bind/lease lifecycle will see `agend-git: ERROR ... HINT: ...` errors when running `git worktree add` or `git checkout main`. The error message names the reason and the override path. This is intentional, but it surprises people the first time.
- **Restart needed to pick up changes.** After upgrading, `cargo build --release` and restart the daemon. The shim binary path is fixed at startup; in-flight agents do not pick up new shim logic until they respawn.

### How to opt out / bypass

Routine operations inside your bound worktree (`status`, `diff`, `log`, `add`, `commit`, `push origin <your-branch>`, `fetch`, `checkout <existing-branch>` within the same repo) **pass through the shim cleanly** — no bypass needed. Try bare `git` first; if the shim denies, the deny message will name the reason.

For the operations that legitimately need bypass:

```bash
# One-off command
AGEND_GIT_BYPASS=1 AGEND_GIT_BYPASS_AGENT=<name> git worktree add ...

# Per-agent persistent override
export AGEND_GIT_BYPASS_AGENT=<name>

# Time-bounded bypass (Unix epoch)
export AGEND_GIT_BYPASS_UNTIL=$(date -v +1H +%s)
```

Skipping the shim skips the safety net (trailer, deny matrix, registry). Use it for explicitly intended overrides (operator manual cleanup, daemon's own internal git ops, etc.) and not as the default.

**Operator's own terminal is not affected.** The PATH injection only happens inside agent PTYs spawned by the daemon. `which git` from your shell still resolves to your normal `git` binary.

### Where to read more

- [`docs/FLEET-DEV-PROTOCOL-v1.md`](docs/FLEET-DEV-PROTOCOL-v1.md) §13 — full bypass guideline
- [`docs/proposals/agend-git-shim.md`](docs/proposals/agend-git-shim.md) — design doc covering Phases 1-5
- PRs [#446](https://github.com/suzuke/agend-terminal/pull/446) (Phase 1 trailer) · [#447](https://github.com/suzuke/agend-terminal/pull/447) (Phase 2 deny matrix) · [#449](https://github.com/suzuke/agend-terminal/pull/449) (Phase 3 lease) · [#454](https://github.com/suzuke/agend-terminal/pull/454) (Phase 4 GC dry-run) · [#455](https://github.com/suzuke/agend-terminal/pull/455) (Phase 5 hotspot)

## What It Does

Spawns AI coding agents (Claude Code, Codex, Kiro, OpenCode, Gemini) as
long-lived PTY processes, each in its own git worktree. A built-in MCP
server lets agents talk to each other — delegate work, request info,
broadcast updates — without glue code. Crashes are survived by auto-
respawn with context handover. Drive the fleet through a multi-tab /
multi-pane TUI, a Telegram channel, or an optional system tray.

## Why Not tmux?

| | tmux + shell scripts | agend-terminal |
|---|---|---|
| Input injection | `send-keys` race conditions | Atomic PTY write |
| Output capture | Screen scraping | VTerm state tracking |
| Agent health | Manual monitoring | Auto-respawn + state detection |
| Multi-agent comms | Custom IPC | Built-in MCP tools |
| Git isolation | Manual worktrees | Auto per-agent worktree |

## Quick Start

```bash
# Demo (no config)
agend-terminal demo

# Interactive setup — detects backends, wires Telegram, writes fleet.yaml
agend-terminal quickstart

# Or hand-write:
cat > ~/.agend/fleet.yaml << 'YAML'
defaults:
  backend: claude
instances:
  dev:
    role: "Developer"
    working_directory: ~/my-project
  reviewer:
    role: "Code reviewer"
    working_directory: ~/my-project

# Optional: bind the fleet to a Telegram group for remote control + alerts.
# `user_allowlist` is REQUIRED to receive outbound notifications (stall /
# crash / CI alerts). Without it, the fleet runs but the bound group is
# muted (fail-closed default — see docs/USAGE.md "Channel: Telegram").
# channel:
#   type: telegram
#   bot_token_env: AGEND_BOT_TOKEN
#   group_id: YOUR_GROUP_ID
#   user_allowlist: [YOUR_TELEGRAM_USER_ID]   # message @userinfobot to get yours
YAML
agend-terminal start
```

## Backends

| Backend | Command | Status |
|---------|---------|--------|
| Claude Code | `claude` | Tested |
| Kiro CLI | `kiro-cli` | Tested |
| Codex | `codex` | Tested |
| OpenCode | `opencode` | Tested |
| Gemini CLI | `gemini` | Tested |

## Learn More

- **Commands** — [`docs/CLI.md`](docs/CLI.md) for the full subcommand reference.
- **MCP tools** — [`docs/MCP-TOOLS.md`](docs/MCP-TOOLS.md) for the 35 agent-to-agent coordination tools.
- **Architecture** — [`docs/architecture.md`](docs/architecture.md) covers git worktree isolation, health monitoring + auto-respawn, Telegram topic lifecycle, and daemon-resident design.
- **Contributing** — [`CONTRIBUTING.md`](CONTRIBUTING.md).
- **Release history** — [`CHANGELOG.md`](CHANGELOG.md).

## License

MIT
