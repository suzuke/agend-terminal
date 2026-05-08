# RCA — issue #548 daemon authority + lifecycle clean-cut

**Date**: 2026-05-09
**Sprint**: 57 Wave 3 PR-1 (Phase 1 — RCA, Path B doc-only)
**Author**: dev
**Reviewer slot**: Tier-1 codex single primary
**Phase 2 IMPL prereq**: this doc — gates dispatch of Wave 3 PR-2 IMPL track
**Source of truth**: `de93bd4` (post-Sprint-57-Wave-2 main HEAD)

---

## TL;DR

Seven audit points covering the daemon's startup mode, discovery, lifecycle,
and the tray's bootstrap path. The operator-settled targets (Q1-Q7 in the
issue body, baked in via general delta `m-20260508161405812769-8`) form the
spec; this RCA pins each against current `main` and recommends a fix shape
for the Phase 2 IMPL.

| # | Audit point | Current | Target | Status | Phase 2 LOC est. |
|---|-------------|---------|--------|--------|--------------------|
| 1 | Default startup mode | `--detached` opt-in | detached default | **GAP** | ~30 |
| 2 | Daemon discovery | lockfile + TCP probe | dual verification | ✓ aligned | 0 (rustdoc only) |
| 3 | Self-supervisor | absent | NOT self-supervisor | ✓ aligned | 0 (rustdoc only) |
| 4 | Lockfile location | one lock + per-PID identity | one canonical lock | ✓ aligned | 0 (clarification) |
| 5 | Migration policy | hard cut-over (no grace) | hard cut-over | ✓ aligned | 0 |
| 6 | Crash recovery | `daemon_stop` event + per-agent killed traces present, but flat reason + no staged TERM/KILL grace | clean shutdown w/ taxonomy | **PARTIAL** | ~20 |
| 7 | Tray bootstrap_daemon | third spawn entry | CLI `start` only | **GAP** | ~50 |

Net Phase 2 IMPL surface: ~300-450 prod LOC + ~150-200 test LOC per general
scope FINAL LOCK estimate. This RCA confirms the estimate is accurate — the
two structural gaps (Q1 + Q7) are surgical, not architectural.

---

## Audit 1 — Default startup mode (Q1: detached default)

### Status quo

`src/main.rs:181-186` defines `Commands::Start` with:

```rust
Start {
    /// Run as detached service (background daemon)
    #[arg(long)]
    detached: bool,
    ...
}
```

`detached: bool` defaults to `false`. The dispatch at `src/main.rs:404-431`
branches:

- `--detached` set → `daemon_spawn::spawn_detached(home, fleet_path)` (forks,
  CLI returns once daemon publishes its run dir)
- `--detached` unset → blocking foreground `cli::start_with_fleet()` /
  `daemon::run()` runs inline, holds the calling shell

### Gap shape

The user-facing surface today: `agend-terminal start` BLOCKS the shell. New
operators following quickstart docs hit the foreground path first, then
discover the `--detached` flag in `--help` later. Q1's target is to invert
the default — `start` runs detached service mode, foreground becomes opt-in
via a new flag (likely `--foreground` or `--no-detach`).

### Recommended fix shape (Phase 2 IMPL)

1. Rename the field semantically: `detached: bool` → `foreground: bool`,
   defaulting to `false`. Or keep `detached` and invert default to `true`
   with a new `--no-detach` opt-out.
2. Update the dispatch branch logic so foreground is the explicit branch.
3. CLI surface change: announce in CHANGELOG; update `docs/USAGE.md` and
   `docs/CLI.md` to lead with `agend-terminal start` (silent service-mode
   start) and document the foreground escape hatch.
4. Update affected tests in `src/bootstrap/` and `src/daemon/mod.rs`
   that assert blocking-foreground semantics.

### Tests gap

`src/bootstrap/mod.rs` and `src/daemon/mod.rs` contain TOCTOU + PID-reuse +
stale-rundir tests that mock `probe_api`. After Q1 inversion, any test that
implicitly relied on "start = foreground" needs the new default. Phase 2
IMPL adds a `start_default_is_detached_service_mode` regression-proof.

---

## Audit 2 — Daemon discovery mechanism (Q2: lockfile + API dual)

### Status quo

Discovery is **already dual-layer** at `src/bootstrap/mod.rs:212-233`
(`try_attach`):

1. **Lockfile + PID-alive check** — `find_active_run_dir()` at
   `src/daemon/mod.rs:50-84` scans `~/.agend/run/<pid>/`, calls
   `is_pid_alive(pid)`, verifies the `.daemon` PID:timestamp file against
   PID recycling.
2. **TCP API probe** — `probe_api()` at `src/ipc.rs:120-128` opens a 200ms
   timeout connection to the daemon's `api.port` to distinguish "daemon
   alive" from "stale rundir from a kill -9'd daemon".

Both checks run before AND after lock acquisition (TOCTOU guard).

### Gap shape

None. Aligned with Q2 target.

### Recommended fix shape (Phase 2 IMPL)

Rustdoc-only patch on `try_attach` codifying that the dual-layer is the
contract — pin the invariant against future single-layer regressions.

---

## Audit 3 — Auto-restart / self-supervision (Q3: NOT self-supervisor)

### Status quo

The daemon does NOT supervise itself. Two clarifications:

- `src/daemon/supervisor.rs` is the **per-agent** supervisor (vterm scan +
  AwaitingOperator emission for hung agents). Not the daemon.
- The crash channel at `src/daemon/mod.rs:261-270` (bounded `crashbus_rx`)
  receives **agent** exit events for the registry to respawn agents. Not the
  daemon itself.

There is no `daemon::respawn_self()` or watchdog-of-self loop anywhere in
`src/daemon/` or `src/bootstrap/`.

### Gap shape

None. Aligned with Q3 target.

### Recommended fix shape (Phase 2 IMPL)

Rustdoc-only patch on `daemon::run_core` codifying "this daemon does NOT
supervise itself; OS service registration via `agend-terminal service
install` (Phase 3) is the supervisor of last resort". Pins the contract for
future readers tempted to add a self-restart loop.

---

## Audit 4 — Lockfile location (Q4: single canonical)

### Status quo

Two file types under `$AGEND_HOME` participate in daemon-lifecycle state:

- **Singular lock** — `$AGEND_HOME/.daemon.lock` (acquired via
  `fs2::FileExt::try_lock_exclusive` at `src/bootstrap/mod.rs:237` and
  `src/daemon/mod.rs:157`). One lock file, one acquirer at a time.
- **Per-PID identity** — `$AGEND_HOME/run/<pid>/.daemon` content
  `pid:start_time` (written at `src/daemon/mod.rs:120-127`). One file per
  daemon process; old PIDs leave directories that get GC'd by the stale-
  rundir cleanup path.

### Gap shape

If Q4 means "one lock acquired by one daemon at a time" — already aligned
(`.daemon.lock` is singular).

If Q4 means "one file per daemon-lifecycle concept" — there are two: the
exclusive lock and the per-PID identity stamp. They serve different roles
(lock = atomic acquirer election; identity = PID-recycling guard for
discovery). Consolidating would lose information.

**Recommendation: clarify Q4 to "one canonical *lock*" rather than "one
*file*"**. The current state already satisfies the lock-uniqueness
invariant.

### Recommended fix shape (Phase 2 IMPL)

Rustdoc-only patch on the lock-acquisition site explaining the lock vs
identity-stamp distinction and pinning that any future "consolidate to one
file" refactor must preserve PID-recycling guard semantics.

---

## Audit 5 — Migration policy (Q5: hard cut-over)

### Status quo

The startup path runs two **data-format** migrations:

- `teams.json → fleet.yaml` at `src/daemon/mod.rs:212-221` (Sprint 54,
  idempotent, `.migrated` marker)
- `tasks.json → task_events.jsonl` at `src/daemon/mod.rs:235-250` (Sprint 24
  P0)

Both are **one-shot, fail-loud** — no dual-path startup, no opt-out, no
deprecation grace period. Re-runs are no-ops.

The home-directory fallback at `src/main.rs:89-101` (`~/.agend` preferred,
fall back to `~/.agend-terminal`) is the only legacy-path artefact in
startup flow. It exists for backward compatibility with operators who
upgraded from pre-Sprint-50 installs and is a stat-once-then-pick path
choice — not a "deprecation buffer".

There is NO startup-path deprecation buffer (e.g. "also accept old
`Commands::Daemon` for one Sprint"). The Sprint 56 Track I-Phase2c hard
removal of `Commands::Mcp` is the precedent for Q5's "hard cut-over"
philosophy.

### Gap shape

None. Aligned with Q5 target.

### Recommended fix shape (Phase 2 IMPL)

For the Q1 default-flip in particular: ship the inversion with NO
`--legacy-foreground-default` opt-out. CHANGELOG entry documents the break.
Mirror the Sprint 56 Track I-Phase2c precedent.

---

## Audit 6 — Crash recovery / graceful exit (Q6: clean shutdown)

### Status quo

Signal handlers present at `src/bootstrap/signals.rs`:

- **Daemon mode** (`install`, lines 25-50): `ctrlc::set_handler()` bundles
  `SIGINT + SIGTERM + SIGHUP` on Unix via the `ctrlc` crate's `termination`
  feature. Handler sets `shutdown: AtomicBool` AND wakes `shutdown_tx`
  channel.
- **App mode** (`install_term_only`, lines 75-104): Unix `sigaction(SIGTERM)`
  only — SIGINT left to crossterm's TUI handler. Windows
  `SetConsoleCtrlHandler` filters to `CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT |
  CTRL_SHUTDOWN_EVENT`.

The main loop at `src/daemon/mod.rs:372` polls `shutdown.load(...)` each
tick and breaks when set. `OwnedFleet` drops on scope exit, releasing the
lockfile via `Drop`.

The cleanup path before exit DOES emit structured signals (the original
draft of this RCA missed these — corrected per reviewer code-cite):

- `event_log::log(home, "daemon_stop", "", "shutdown")` is the structured
  shutdown event already emitted on the cleanup path.
- Per-agent kill traces: `tracing::info!(agent = %name, "killed")` fires
  for each agent the daemon takes down during cleanup.
- Daemon-level traces: `tracing::info!("cleaning up...")` and
  `tracing::info!("exiting")` bracket the cleanup window.

### Gap shape

The structured `daemon_stop` event + per-agent killed traces + bracket
log lines are present today, so the gap is NOT "no event / no traces".
The real shortfall is in shutdown semantics:

- **Graceful staged termination contract is missing** — there is no
  TERM grace window (e.g. send SIGTERM, wait up to 2s, then SIGKILL the
  holdouts). The current path falls through to whatever the per-agent
  kill code does, with no explicit grace ladder.
- **Reason taxonomy is flat** — `daemon_stop` carries the literal
  `"shutdown"` string. There's no categorization of WHY the daemon
  stopped (clean exit / SIGINT / SIGTERM / SIGHUP / panic / lockfile
  conflict / fleet-yaml reload abort / etc.). Operators reading the
  event log can't easily slice "how often did this daemon get
  SIGTERM'd vs exit cleanly?".
- **Summary telemetry is sparse** — no per-shutdown rollup of
  "agents killed: N", "uptime: X seconds", "agents that hit the kill
  window vs exited cleanly", etc. Tracing emits per-agent lines but
  the aggregate footprint requires log-grepping to reconstruct.

### Recommended fix shape (Phase 2 IMPL)

Add a `daemon::run_core::shutdown_sequence()` block before the main-loop
break that ENRICHES the existing `daemon_stop` path rather than inventing
a parallel event:

1. Logs `tracing::info!(agents = n, "daemon shutdown: stopping agents")`.
2. Iterates the `AgentRegistry`, sends SIGTERM to each agent's PTY,
   waits up to 2s, then SIGKILL the holdouts (the staged termination
   ladder Q6 calls for). Track per-agent disposition (clean / killed
   after grace) for the rollup.
3. Continues to call `event_log::log(home, "daemon_stop", ...)` —
   keeps the event NAME stable so existing operator queries / greps /
   audit-trail downstreams keep working — but ENRICHES the payload
   (third arg) with structured reason taxonomy + summary metrics:
   - `reason: "sigint" | "sigterm" | "sighup" | "clean_exit" | "panic"`
   - `agents_total: N`, `agents_killed_after_grace: M`,
     `uptime_secs: U` (or whatever subset proves useful first;
     additive payload extensions are forward-compatible)
4. Flushes any in-memory state (notification queue checkpoint, dedup
   ledger persistent saves are already incremental — Sprint 57 Wave 2
   Track C #553).

The decision to keep the event name `daemon_stop` (not rename to
`daemon_shutdown`) is deliberate per Phase 2 review-thread guidance:
preserves existing query / grep / downstream-consumer paths while
giving operators richer payload semantics.

### Tests gap

`src/bootstrap/signals.rs::tests` has signal-handler installation tests but
no shutdown-sequence empirical pin. Phase 2 IMPL adds:

- `shutdown_logs_agent_count_before_termination`
- `daemon_stop_event_payload_carries_reason_and_metrics`
  (asserts the enriched payload shape, NOT a new event name)
- `agent_kill_window_respects_2s_grace_period_then_sigkill`

---

## Audit 7 — Tray bootstrap_daemon path (Q7: CLI `start` only spawn entry)

### Status quo

Three spawn entries currently exist:

1. **CLI `start --detached`** — explicit operator command (`src/main.rs:404`)
2. **App mode** — `bootstrap::prepare()` (`src/app/mod.rs:156-192`) returns
   `Owned` if it acquired the lock and spawned, `Attached` if it joined
   an existing daemon
3. **Tray** — `bootstrap_daemon()` at `src/tray/mod.rs:123-135` probes via
   `api::call(LIST)`; on failure calls
   `daemon_spawn::spawn_detached(home, None)` directly

The tray was added in PR #17 era (Sprint <=50 timeframe). Its
`bootstrap_daemon` was a convenience for the system-tray UX so clicking the
tray icon would also start the daemon if missing.

### Gap shape

Tray is the third spawn entry. Q7's target consolidates daemon spawn to CLI
`start` only:

- Tray's role becomes **status widget + GUI launcher**:
  - Probes daemon via `api::call`.
  - If alive → renders status (agent count, channel, recent events).
  - If absent → renders a "Click to start daemon" menu item that invokes
    `Command::new(env::current_exe()).arg("start")` (i.e. the tray
    spawns the CLI `start` command, not the daemon directly).
- Tray's supervisor role (already mostly absent — it doesn't auto-restart
  the daemon) is fully removed.

App mode's `bootstrap::prepare()` is a separate concern: app mode is the
operator's interactive session. Q7's "CLI `start` only" should be read as
"the production startup path; app mode's owned-daemon-on-failure is a
secondary path for demo/quickstart and stays".

### Recommended fix shape (Phase 2 IMPL)

1. Delete `src/tray/mod.rs::bootstrap_daemon` and the `daemon_spawn`
   import within tray.
2. Replace with:
   ```rust
   fn check_daemon_state(home: &Path) -> DaemonState {
       match api::call(home, ApiRequest::List) {
           Ok(_) => DaemonState::Running,
           Err(_) => DaemonState::Offline,
       }
   }
   ```
3. Tray menu rendering branches on `DaemonState`:
   - `Running` → "Stop daemon" + "Open dashboard" entries (current shape)
   - `Offline` → "Click to start daemon" entry which spawns
     `Command::new(env::current_exe()).arg("start").spawn()` (NOT
     direct `daemon_spawn`).
4. Tray drops the implicit assumption that it OWNS daemon lifecycle. The
   tray becomes a thin client.

### Tests gap

Tray tests at `src/tray/mod.rs::tests` (if any exist — confirm during Phase
2 IMPL recon) need:

- `tray_spawns_via_cli_start_not_direct_daemon_spawn` regression-proof
- `tray_offline_state_renders_start_menu_item`
- `tray_running_state_renders_stop_and_dashboard_items`

---

## Phase 2 IMPL prereq summary

For the Phase 2 IMPL dispatch, the per-audit dependencies are:

| # | Module surfaces | Type of change | Migration data |
|---|-----------------|----------------|------------------|
| 1 | `src/main.rs::Commands::Start` + dispatch branch + USAGE.md/CLI.md | semantic flip | none (fail-loud break) |
| 2 | `src/bootstrap/mod.rs::try_attach` | rustdoc | none |
| 3 | `src/daemon/mod.rs::run_core` | rustdoc | none |
| 4 | `src/daemon/mod.rs:120` + `src/bootstrap/mod.rs:237` | rustdoc | none |
| 5 | n/a (informs Q1's no-grace cut-over) | - | - |
| 6 | new `daemon::shutdown_sequence` (TERM-then-KILL grace ladder) + ENRICH existing `daemon_stop` event payload (reason taxonomy + summary metrics; event NAME unchanged for downstream-consumer continuity) | additive | none |
| 7 | delete `src/tray/mod.rs::bootstrap_daemon`; new `check_daemon_state` + menu render branches | structural | none |

Total: ~300-450 prod LOC + ~150-200 test LOC, single Phase 2 PR per general
scope FINAL LOCK. Tier-1 baseline (no Tier-2 dual review needed —
persistence-design dimensions don't apply here).

Suggested Phase 2 dispatch order within the PR:

1. **Audit 1** (default-flip) — biggest user-facing impact, get reviewer
   eyes on it first.
2. **Audit 7** (tray consolidation) — second-biggest structural change.
3. **Audit 6** (graceful-shutdown enrichment) — additive, lower risk.
4. **Audits 2/3/4** (rustdoc-only contract pins) — landed last as part of
   the same PR.

---

## Verdict — Phase 1 status

Seven audit points complete. Two structural gaps (Q1 + Q7), one partial
(Q6), four aligned. Phase 2 IMPL surface is ~surgical and matches general
scope FINAL LOCK estimate.

Phase 2 IMPL clear to dispatch.

---

## Out of scope — not addressed in Phase 1

- **Phase 2 IMPL** — Wave 3 PR-2 separate dispatch
- **Phase 3 service helper** — `agend-terminal service install/uninstall/
  status` for macOS launchd / Linux systemd user / Windows Task Scheduler;
  Wave 3 PR-3, Tier-2 dual review per general scope FINAL LOCK
- **#546 Item 4 worktree placement** — Wave 4 single PR, parallel-feasible
  filler

## References

- issue #548: daemon authority + lifecycle clean-cut (filed by general
  during operator absence)
- general delta `m-20260508161405812769-8`: Q7 tray strategy added
- general scope FINAL LOCK `m-20260508161737936234-14`: Wave 3 + Wave 4
  scope
- Sprint 57 PLAN draft `m-20260508155148...`: Wave 3 spec
- Sprint 56 Track I-Phase2c (PR #547): hard-removal precedent for Q5
  cut-over philosophy
- Sprint 57 Wave 2 Track C (PR #553): incremental persistence pattern
  referenced for Audit 6 state-flush design
