[繁體中文](GIT-BEHAVIOR.zh-TW.md)

# Git Behavior Modification

agend-terminal does **not** run AI agents against vanilla `git`. To coordinate multiple agents safely on the same repo, the daemon installs a thin shim layer between agents and your real `git` binary. **Read this page before starting the daemon** — once you start it, the modifications below take effect for every spawned agent.

Your own terminal is **not affected**. The PATH injection only happens inside agent PTYs spawned by the daemon. `which git` from your shell still resolves to your normal `git` binary.

## What gets modified

- **PATH shim for agent processes.** A symlink at `$AGEND_HOME/bin/git` points to the daemon-selected git guard. With `use_agentic_git_shim` enabled, the git-capable target is the vendored `agentic-git` binary. The in-tree `agend-git` binary is now the kill-family guard and a fail-closed fallback; it no longer handles git commands. When the daemon spawns an agent PTY, the shim directory is prepended to `PATH`, so agent git operations are checked before the guard invokes the real `git`.
- **Per-worktree commit hooks.** For agent-managed worktrees, the daemon points `core.hooksPath` to `$AGEND_HOME/hooks` and installs a `prepare-commit-msg` hook that appends `Agend-Agent`, `Agend-Branch`, `Agend-Issued-At`, and (when present) `Agend-Task` trailers to the commit message. Trailers are skipped if already present (idempotent).
- **Deny matrix on agent git ops.** The shim refuses certain commands from unbound or cross-branch contexts: `git worktree add/remove/move`, `git checkout` of a different branch, etc. The daemon owns the worktree pool. The original design is preserved as history in [`docs/archived/proposals/agend-git-shim.md`](archived/proposals/agend-git-shim.md); current behavior is defined by the live guard and [`FLEET-DEV-PROTOCOL.md`](FLEET-DEV-PROTOCOL.md) §10, §12.4, and §13.
- **Auto bind/lease on dispatch (or via `bind_self`).** When you delegate a task to an agent with a `branch` field, the daemon auto-creates a managed worktree, marks it with a `.agend-managed` file, and writes a `binding.json` recording the agent → branch link.
- **Worktree lifecycle is daemon-managed.** Cleanup is via the `release_worktree` MCP tool, not direct `git worktree remove`. A daemon-side **hourly** GC sweep (`gc_tick`) then **auto-removes** daemon-managed worktrees once they have been released, are past the grace period, and are neither pinned nor bound — force-reclaim candidates are archived to a recoverable `.trash` rather than hard-deleted, and stale ci-watch locks are swept too. (A removed or archived worktree takes its `target/` with it; additionally, the sweep age-reclaims the stale `target/` build dir of a `.agend-managed` `home/worktrees` worktree that is NOT currently bound — i.e. its owner instance is gone from the roster, or is in the roster but bound elsewhere/unbound — keeping the worktree itself. It **never** reclaims a currently-bound worktree (regardless of the owner's run state — that owner could start a build at any instant; the delete is fenced by holding the owner's `.binding.json.lock`), and **never** touches markerless `workspace/<agent>/target` or agent-self-built `.claude/worktrees/*/target`.) Use `agend-terminal admin gc-dry-run` (#2548: moved from the `gc_dry_run` MCP tool) for a non-destructive preview of what would be removed.

## Why

- **Multi-agent safety.** Multiple AI agents working in the same repo without isolation will race on the same branch. Per-agent worktrees make that impossible at the git layer rather than relying on agent-side discipline.
- **Audit trail.** `Agend-Agent: <name>` trailer answers "which agent made this commit?" without parsing chat logs. Useful when reviewing autonomous work, much more useful when something went wrong.
- **Lifecycle hygiene.** Crashed agents, stale dispatches, and abandoned branches accumulate fast in a multi-agent setup. The daemon's bind/lease/release gives the cleanup work a single owner.
- **Safety guard rails.** The deny matrix catches the obvious foot-guns (agents accidentally checking out `main`, deleting other agents' worktrees) at the shim layer instead of after the fact.

## Risk

- **Agents see a different `git` than you do.** The PATH injection only happens inside agent PTYs spawned by the daemon. Your own terminal's `git` is unchanged. To compare guarded and bare-git behavior, inspect the same refs from your unaffected operator terminal; do not disable the guard from an agent PTY.
- **Commits gain extra trailers.** Tools that parse commit messages strictly (some changelog generators, some CLA bots) may need their parsers updated. Standard `git log --format` output is unaffected; the trailers are appended after the commit body.
- **Some commands deny unexpectedly.** A new agent or operator unfamiliar with the bind/lease lifecycle may see a guard error when attempting raw worktree lifecycle operations or switching to a protected branch. The error names the reason and the daemon-managed remediation. Treat the denial as a protocol signal, not permission to retry underneath the guard.
- **Restart needed to pick up changes.** After upgrading, `cargo build --release` and restart the daemon. The shim binary path is fixed at startup; in-flight agents do not pick up new shim logic until they respawn.

## When the guard denies an operation

Routine operations inside your bound worktree (`status`, `diff`, `log`, `add`, `commit`, `push origin <your-branch>`, and `fetch`) **pass through the shim cleanly**. Run normal `git`; do not prefix it with a bypass variable.

If the guard denies an operation:

1. Stop and read the denial's remediation.
2. Use daemon-owned lifecycle operations such as `repo` checkout,
   `bind_self`, or `release_worktree`; never run raw `git worktree` lifecycle
   commands from an agent.
3. If no safe route applies, ask the lead or operator. Bypass variables are
   reserved for daemon internals and an explicitly authorized, audited
   one-command repair after normal recovery routes are exhausted. Agents must
   not set them on their own.

## Where to read more

- [`docs/FLEET-DEV-PROTOCOL.md`](FLEET-DEV-PROTOCOL.md) §13 — full bypass guideline
- [`docs/archived/proposals/agend-git-shim.md`](archived/proposals/agend-git-shim.md) — historical design covering Phases 1–5
- PRs [#446](https://github.com/suzuke/agend-terminal/pull/446) (Phase 1 trailer) · [#447](https://github.com/suzuke/agend-terminal/pull/447) (Phase 2 deny matrix) · [#449](https://github.com/suzuke/agend-terminal/pull/449) (Phase 3 lease) · [#454](https://github.com/suzuke/agend-terminal/pull/454) (Phase 4 GC dry-run) · [#455](https://github.com/suzuke/agend-terminal/pull/455) (Phase 5 hotspot)
