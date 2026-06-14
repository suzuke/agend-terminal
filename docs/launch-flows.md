[繁體中文](launch-flows.zh-TW.md)

# Launch Flows

This doc maps every way to start `agend-terminal`, the daemon lifecycle each
path implies, and how cold-start (no daemon running) differs from warm-start
(a daemon is already up).

Current as of `fe528c1` (post-#879v3 revert).

## TL;DR matrix

| Command | Cold start (no daemon) | Warm start (daemon up) | Shell behavior |
|---|---|---|---|
| `agend-terminal start` | spawn daemon detached to background; parent exits | bail: `another agend-terminal daemon is already running (lock held)` | non-blocking |
| `agend-terminal start --foreground` | run daemon in foreground; blocks the shell | bail (same) | blocks; Ctrl+C kills daemon |
| `agend-terminal start --agents NAME:CMD ...` | implies `--foreground`; skips `fleet.yaml`; spawns only the listed agents | bail | blocks |
| `agend-terminal app` | **Owned mode**: TUI brings up an in-process daemon | **Attached mode**: TUI runs as a client of the existing daemon | blocks; Ctrl+B d behavior differs (see below) |
| `agend-terminal tray` | menu bar app idles; "Start daemon" menu item shells out to `agend-terminal start --foreground` | menu items reflect daemon state; no spawn | resident; doesn't tie up a shell |

Auxiliary subcommands (not full launches, but lifecycle-relevant):

- `attach <name>` — connect to an existing agent's PTY in the current
  shell. Ctrl+B d detaches back to the shell.
- `connect <name> --backend X` — register a new agent with the running daemon.
- `stop` — clean shutdown of the daemon.
- `kill <name>` — stop a single agent.
- `list` (alias `status`, `ls`) — list running agents.

## Daemon discovery — Owned vs Attached

Both `start` and `app` go through the same `bootstrap::prepare()` seam in
`src/bootstrap/mod.rs`. It returns either:

- `BootstrapOutcome::Attached(_)` — a live daemon owns the run dir; the
  caller plugs in as a client.
- `BootstrapOutcome::Owned(_)` — no live daemon; the caller IS the daemon
  for the duration of its process.

The decision is made in 4 steps:

1. `try_attach()` — scan `~/.agend-terminal/run/*`, probe `api.port` (TCP
   connect, 200ms timeout). If the probe succeeds, return Attached.
2. Acquire the exclusive daemon lock (`acquire_daemon_lock`). This blocks
   other starters from racing.
3. Re-run `try_attach()` (TOCTOU guard — another daemon could have come
   up between step 1 and step 2).
4. Still no live daemon → create the run dir, write `.daemon` identity,
   issue `api.cookie`, load `fleet.yaml`, and return Owned.

`start` and `app` differ only in what they do with the outcome:

- `start` mode: runs purely as a daemon. No TUI.
- `app` mode: starts the TUI; if Owned, also runs the in-process API
  server. If Attached, runs `noop_guard` (no API server) and treats the
  daemon as authoritative.

## The asymmetry that bites operators (issue #879)

`app` mode behaves differently depending on whether you cold-start or
attach:

- **Cold start (Owned)** — daemon lives inside the TUI process. Hitting
  Ctrl+B d (or otherwise exiting the TUI) terminates the whole process,
  which kills the daemon and every agent PTY along with it.
- **Warm start (Attached)** — daemon is a separate process. Ctrl+B d
  only ends the TUI; daemon + agents keep running. Re-launching `app`
  re-attaches.

Operators expect Ctrl+B d to always be safe — i.e. `app` should always
take the Attached path, auto-spawning a detached daemon during cold
start so the asymmetry disappears.

PR #903 (#879v3) attempted this and was reverted (`fe528c1`) after
hitting two pre-existing race bugs that the previous Owned-mode masked
(see issue #879 — #879v4 is the follow-up fix for those races, not the
always-Attached pivot itself).

## Tray separation contract (#548 Q7)

`tray` (menu bar app, gated on the `tray` feature) never touches daemon
internals. Its "Start daemon" menu item shells out:

```text
Command::new("agend-terminal").arg("start").arg("--foreground")
```

— equivalent to typing `agend-terminal start --foreground` in another
shell. The tray process never holds the daemon lock and never speaks to
the API directly. This separation is the #548 Q7 contract; do not have
the tray bypass it.

## `--foreground` default and the fork-bomb hotfix

`start` defaults to detached service mode. `--foreground` is the
opt-out for operators who want to keep the daemon attached to the
shell — useful when debugging daemon logs or running under
systemd/launchd/Task Scheduler.

`spawn_detached` (in `src/bootstrap/daemon_spawn.rs`) forks the
current binary as `agend-terminal start --foreground ...` — passing
`--foreground` is required, otherwise the child re-enters the
default-detach branch and recursively spawns itself.

## See also

- `src/bootstrap/mod.rs::prepare` — Owned/Attached decision logic.
- `src/bootstrap/daemon_spawn.rs` — detached-spawn implementation.
- `src/tray/mod.rs::start_daemon_via_cli` — tray's CLI shell-out.
- `src/app/mod.rs::run_app` — app's bootstrap consumer.
- Issue #879 — always-Attached pivot (in progress).