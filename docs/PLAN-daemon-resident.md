# Plan: daemon-resident bootstrap (tmux-style)

Status: Stages 1 & 2 shipped on main (commits `8a1107e..ece0933`, 2026-04-20,
pushed). Stage 3 is partially scoped and partially shipped — see §3.

## Why this exists

Before this arc there were two independent preflight paths:

- `cli::start_with_fleet` (daemon): acquire `.daemon.lock`, create run dir,
  write `.daemon`, issue `api.cookie`, normalize fleet.yaml, resolve agents,
  init Telegram.
- `app::run` (TUI): almost the same — but **missed** `auth_cookie::issue`.
  Result: `api::serve` at `src/api.rs:123` read `api.cookie`, found nothing,
  logged `api.cookie missing; aborting serve`, and every
  `inbox::notify_agent` → `api::call(INJECT)` from Telegram silently dropped.

The root fix is structural: one seam, two consumers. Both callers route
through `bootstrap::prepare(home, fleet_path, opts) -> BootstrapOutcome`.
`BootstrapOutcome::Owned` means we hold the flock and cookie; `::Attached`
means another daemon owns them and we speak to it as a client.

The secondary goal is tmux-style lifecycle: `agend-terminal start --detached`
forks the daemon into the background, survives shell exit, and `app` can
connect to it without spawning competing local PTYs.

## Layout

```
src/bootstrap/
├── mod.rs               # prepare() + OwnedFleet / AttachedFleet / DaemonLock
├── agent_resolve.rs     # FleetConfig → Vec<AgentDef>
├── fleet_normalize.rs   # general auto-create, topic_id backfill, prune worktrees
├── telegram_init.rs     # spawn polling thread when configured
├── signals.rs           # ctrlc SIGINT+SIGTERM+SIGHUP → shared shutdown flag
└── daemon_spawn.rs      # spawn_detached: fork {current_exe} start, process_group(0)
```

`daemon::write_daemon_id` / `daemon::read_daemon_pid` are `pub(crate)` so
all four call sites (bootstrap preflight, daemon_spawn readiness poll,
app pre-TUI check, daemon attach lookup) share one implementation.

## Shipped stages

### Stage 1 — shared preflight seam
| Commit | Change |
|---|---|
| `8a1107e` | `bootstrap::prepare` + `BootstrapOutcome::Owned/Attached` |
| `92205da` | `cli::start_with_fleet` reduced from ~145 LOC to ~15, delegates to `prepare` |
| `e934269` | **Bug fix**: `app::run` routed through `prepare` so `api.cookie` gets issued before `api::serve` starts |
| `f35ce64` | Regression tests: `owned_cookie_is_readable_for_api_serve`, `attach_fails_when_cookie_missing` |
| `46dfd25` | `signals::install` extracted; ctrlc feature `termination` enables SIGTERM + SIGHUP bundled with SIGINT |

### Stage 2 — detached daemon + app fail-fast
| Commit | Change |
|---|---|
| `aef61d2` | `start --detached` via `bootstrap::daemon_spawn::spawn_detached` (tmux-style, `process_group(0)` on unix, `DETACHED_PROCESS \| CREATE_NEW_PROCESS_GROUP` on windows, logs to `$AGEND_HOME/daemon.log`). App mode bails pre-TUI when another daemon is active, pointing users to `agend-terminal attach <name>`. |
| `ece0933` | Simplify pass: dedupe `write_daemon_id` / `read_daemon_pid`, avoid `agents.clone()` in `run_with_prepared` via `mem::take`, strip narrating comments. |

**Not** done in Stage 2 (intentional): BackgroundServices abstraction. App
already has `pane_factory` for its own PTYs and doesn't need the daemon's
respawn/snapshot/supervisor plumbing other than `supervisor::spawn` (which
is already called directly from `app::run_app`).

## Stage 3 — app as first-class attach client

**Goal.** When a daemon is already running, `agend-terminal app` should
*connect* to it (remote pane per agent, live vterm feed) rather than bail.
Today's fail-fast is a compromise because the pane layer only understands
local PTYs.

### 3.1 — extract `bridge_client` from `src/tui.rs` [DONE in `a3340b6`]
`src/bridge_client.rs` owns the connect + cookie + protocol-version
handshake and the framed send side. The read side is exposed as an owned
`TcpStream` (via `take_reader()`) so each consumer can park its own thread.
`tui::attach` keeps raw-mode + crossterm polling and delegates network
plumbing to `BridgeClient`. No new consumers added in that commit.

### 3.2 — `Pane::Local` / `Pane::Remote` abstraction [DONE]
`layout::PaneSource` enum with `Local` (routes through `AgentRegistry` by
`agent_name`) and `Remote(Arc<Mutex<BridgeClient>>)` (routes through the
pane's own bridge client). `Pane::write_input` and `Pane::resize_pty`
dispatch on source. `render::resize_panes` uses `pane.resize_pty` instead
of inlining `handle.pty_master.resize`. No Remote consumer yet — that's
3.4. The variant is marked `#[allow(dead_code)]` until then.

### 3.3 — `spawn_detached` [DONE in Stage 2]
Already shipped (`aef61d2`). Listed here for traceability.

### 3.4 — app Owned vs Attached branches [PARTIAL]
- Owned: today's behavior, untouched.
- Attached (today): fail-fast with a message.
- Attached (3.4 goal): open a tab per agent reachable from the daemon,
  each pane a `Pane::Remote` connected via `bridge_client`. 3.1 and 3.2
  are now landed — only app wiring remains.

### 3.5 — SIGTERM-only handler for app [DONE in `a9df3a8`]
`bootstrap::signals::install_term_only` — Unix `libc::sigaction(SIGTERM)`
with `SA_RESTART`, Windows `SetConsoleCtrlHandler` filtered to
`CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT`. SIGINT is
left to crossterm so Ctrl+C still reaches the focused PTY as `0x03`. Main
loop polls `signals::term_requested()` each 50ms tick.

### 3.6 — lifecycle e2e tests [NOT DONE]
Shell-script driven, not unit tests. Scenarios:
1. `start --detached` → parent exits → daemon still alive, Telegram
   still delivers.
2. Second `start` hits the flock → exits with helpful message.
3. `app` while daemon is live → today: fail-fast; post-3.4: connects.
4. `stop` → daemon tears down run dir; subsequent `start` cold-starts.

Tracking: `scripts/e2e/daemon-lifecycle.sh` (does not exist yet).

## Out of scope for this plan

- Cross-host remote (agend-terminal talking to a daemon on another
  machine). The `bridge_client` protocol is still TCP + cookie + framing,
  so it would extend naturally, but security review is required first.
- Unified BackgroundServices abstraction — see Stage 2 note.
- Hot-reload of fleet.yaml in the Owned daemon — orthogonal; tracked
  separately in `agend-pty` catchup branch.

## Dead-code warnings on `OwnedFleet` / `AttachedFleet`

`OwnedFleet::fleet_path`, `cookie`, `lock` and `AttachedFleet::home`,
`fleet_path`, `cookie` are currently unread. They are scaffolding for
Stage 3 (Attached client will need `cookie` + `fleet_path`; Owned tests
for the follow-on work will read `cookie` via API). Leaving the warnings
visible until Stage 3 consumers land is deliberate — suppressing them
with `#[allow(dead_code)]` would hide the scaffolding at exactly the
moment we need to remember it exists.
