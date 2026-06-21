[繁體中文](GIT-BEHAVIOR.zh-TW.md)

# Git Behavior Modification

agend-terminal does **not** run AI agents against vanilla `git`. To coordinate multiple agents safely on the same repo, the daemon installs a thin shim layer between agents and your real `git` binary. **Read this page before starting the daemon** — once you start it, the modifications below take effect for every spawned agent.

Your own terminal is **not affected**. The PATH injection only happens inside agent PTYs spawned by the daemon. `which git` from your shell still resolves to your normal `git` binary.

## What gets modified

- **PATH shim for agent processes.** A symlink at `$AGEND_HOME/bin/git` points to a small Rust binary (`agend-git`). When the daemon spawns an agent's PTY, that path is prepended to the agent's `PATH`. Agents that invoke `git` end up running the shim; the shim forwards almost every command to your real `git` (resolved via `AGEND_REAL_GIT` or `which`).
- **Per-worktree commit hooks.** For agent-managed worktrees, the daemon points `core.hooksPath` to `$AGEND_HOME/hooks` and installs a `prepare-commit-msg` hook that appends `Agend-Agent`, `Agend-Branch`, `Agend-Issued-At`, and (when present) `Agend-Task` trailers to the commit message. Trailers are skipped if already present (idempotent).
- **Deny matrix on agent git ops.** The shim refuses certain commands from unbound or cross-branch contexts: `git worktree add/remove/move`, `git checkout` of a different branch, etc. The daemon owns the worktree pool — see Phase 3 lease in [`docs/proposals/agend-git-shim.md`](proposals/agend-git-shim.md).
- **Auto bind/lease on dispatch (or via `bind_self`).** When you delegate a task to an agent with a `branch` field, the daemon auto-creates a managed worktree, marks it with a `.agend-managed` file, and writes a `binding.json` recording the agent → branch link.
- **Worktree lifecycle is daemon-managed.** Cleanup is via the `release_worktree` MCP tool, not direct `git worktree remove`. A daemon-side **hourly** GC sweep (`gc_tick`) then **auto-removes** daemon-managed worktrees once they have been released, are past the grace period, and are neither pinned nor bound — force-reclaim candidates are archived to a recoverable `.trash` rather than hard-deleted, and stale ci-watch locks are swept too. (A removed or archived worktree takes its `target/` with it; additionally, the sweep age-reclaims the stale `target/` build dir of a `.agend-managed` `home/worktrees` worktree that is NOT currently bound — i.e. its owner instance is gone from the roster, or is in the roster but bound elsewhere/unbound — keeping the worktree itself. It **never** reclaims a currently-bound worktree (regardless of the owner's run state — that owner could start a build at any instant; the delete is fenced by holding the owner's `.binding.json.lock`), and **never** touches markerless `workspace/<agent>/target` or agent-self-built `.claude/worktrees/*/target`.) Use the `gc_dry_run` MCP tool for a non-destructive preview of what would be removed.

## Why

- **Multi-agent safety.** Multiple AI agents working in the same repo without isolation will race on the same branch. Per-agent worktrees make that impossible at the git layer rather than relying on agent-side discipline.
- **Audit trail.** `Agend-Agent: <name>` trailer answers "which agent made this commit?" without parsing chat logs. Useful when reviewing autonomous work, much more useful when something went wrong.
- **Lifecycle hygiene.** Crashed agents, stale dispatches, and abandoned branches accumulate fast in a multi-agent setup. The daemon's bind/lease/release gives the cleanup work a single owner.
- **Safety guard rails.** The deny matrix catches the obvious foot-guns (agents accidentally checking out `main`, deleting other agents' worktrees) at the shim layer instead of after the fact.

## Risk

- **Agents see a different `git` than you do.** The PATH injection only happens inside agent PTYs spawned by the daemon. Your own terminal's `git` is unchanged. But if you compare what an agent did against `git log` from your shell, the agent's command went through the shim and the shim may have intercepted it. Set `AGEND_GIT_BYPASS=1` if you need to reproduce an agent's exact bare-`git` behavior.
- **Commits gain extra trailers.** Tools that parse commit messages strictly (some changelog generators, some CLA bots) may need their parsers updated. Standard `git log --format` output is unaffected; the trailers are appended after the commit body.
- **Some commands deny unexpectedly.** A new agent or operator unfamiliar with the bind/lease lifecycle will see `agend-git: ERROR ... HINT: ...` errors when running `git worktree add` or `git checkout main`. The error message names the reason and the override path. This is intentional, but it surprises people the first time.
- **Restart needed to pick up changes.** After upgrading, `cargo build --release` and restart the daemon. The shim binary path is fixed at startup; in-flight agents do not pick up new shim logic until they respawn.

## How to opt out / bypass

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

## Where to read more

- [`docs/FLEET-DEV-PROTOCOL.md`](FLEET-DEV-PROTOCOL.md) §13 — full bypass guideline
- [`docs/proposals/agend-git-shim.md`](proposals/agend-git-shim.md) — design doc covering Phases 1–5
- PRs [#446](https://github.com/suzuke/agend-terminal/pull/446) (Phase 1 trailer) · [#447](https://github.com/suzuke/agend-terminal/pull/447) (Phase 2 deny matrix) · [#449](https://github.com/suzuke/agend-terminal/pull/449) (Phase 3 lease) · [#454](https://github.com/suzuke/agend-terminal/pull/454) (Phase 4 GC dry-run) · [#455](https://github.com/suzuke/agend-terminal/pull/455) (Phase 5 hotspot)