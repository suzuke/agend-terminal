# agend-terminal — Claude working notes

## Rust workflow

Before committing any Rust change, **always** run:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
```

CI runs these in the first two steps of `ci.yml`. Skipping them locally means
the next push fails and needs an extra "fix fmt / fix clippy" round trip.

### Before `git push`: run the full CI-parity preflight

```bash
scripts/preflight.sh          # full matrix; --quick skips the Windows check
```

This is the one-shot mirror of CI's `check` job and the best way to avoid a
local-green → CI-red round trip. It runs `cargo fmt --check`,
`cargo clippy --all-targets --features tray -- -D warnings`,
`cargo test --tests --features tray` (unit + integration + invariants), and a
**Windows cross-check** (`x86_64-pc-windows-msvc`) — the keystone, since
Windows-only code (`libc::getppid`, `/bin/sh` spawns, `UnixStream`) compiles
fine on a unix dev box but breaks CI's `windows-latest` runner.

The Windows step needs the MSVC C toolchain because a transitive C dependency
(`ring`) won't otherwise cross-compile on macOS/Linux. Install it once:

```bash
cargo install cargo-xwin && rustup target add x86_64-pc-windows-msvc
```

Without `cargo-xwin` the Windows step SKIPs with a hint (never false-fails);
CI's `windows-latest` runner stays the backstop. The preflight is intentionally
*not* a git hook — the full matrix takes a few minutes; run it manually.

A pre-commit hook at `.git/hooks/pre-commit` auto-formats staged `.rs` files
and re-stages them. It does NOT run clippy — clippy is too slow for a
pre-commit path. Run clippy yourself before `git push`.

A pre-push hook verifies push claims (e.g. "no other changes", "deps
unchanged") against the actual diff. Override with `git push --no-verify`
in emergencies.

A post-merge hook triggers a background `cargo build --release` when `src/`
files change. Desktop notification on completion. Does not auto-restart the
daemon — operator decides restart timing. Disable with
`git config core.hooksPath /dev/null`.

### If the hook isn't installed

Hooks are per-clone. After a fresh `git clone`, install with:

```bash
scripts/install-hooks.sh
```

## Test fidelity: feed consumers the producer's real output (#1493)

When a test exercises a consumer of some wire format — a parser, a matcher, a
predicate over a string/struct another function builds — **construct the input
by calling the producer, not by hand-writing the shape.** Review check: *"is
this fixture identical to what production actually sends?"*

This is the #1483 false-green class: `notification_is_actionable_wake` matched
bracketed body markers that the real `[AGEND-MSG-PENDING]` pointer never
contains, and the tests hand-crafted that never-emitted shape — so the matcher
was dead code while the tests passed (then drifted again when #1487 added a
`now=` field the crafted strings lacked). Fix pattern: extract one builder
(e.g. `build_pending_pointer`) and route BOTH production and the tests through
it. Hand-crafted input is still correct for testing a parser's *malformed/edge*
handling — just pin the happy-path contract against the real producer.

## sync→async bridge: no raw shared-runtime `block_on` (#1476)

**HARD RULE.** A sync→async bridge MUST NOT call `block_on` directly on a
long-lived shared runtime accessor (`telegram_runtime()`, `discord_runtime()`,
`shared_ci_runtime()`, …). Those are `current_thread` runtimes, so
`<name>_runtime().block_on(...)` panics with *"Cannot start a runtime from
within a runtime"* the moment a caller reaches it from inside a tokio runtime.

This is exactly the bug telegram hit (#1474, teloxide 0.17 made the path
reachable → panic on the next daemon restart) and that discord had copy-pasted
(#1476). It stays latent for weeks because it only fires when the *caller's*
context changes, not the bridge's.

**Required pattern**: route every value-returning shared-runtime call through a
`block_on_value`-style helper that guards with
`tokio::runtime::Handle::try_current().is_ok()` and, when already inside a
runtime, runs the future on a `std::thread::scope` thread with a *fresh* runtime
(never nested). See `src/channel/telegram/state.rs::block_on_value` and
`src/channel/discord.rs::block_on_value`.

Local, freshly-built runtimes (`let rt = Builder::…build()?; rt.block_on(…)`)
are exempt — a non-shared runtime is never nested. Only the shared-accessor
form is dangerous.

Enforced by `tests/block_on_runtime_guard_invariant.rs`: any
`<name>_runtime().block_on` not inside a Handle-guarded / scoped-thread helper
fails CI. Adding a new channel/bridge? Add a guarded helper, never a raw
`block_on`.

## Worktree branch policy

When creating a worktree, **always use `-b <dedicated-branch>`** to create a
fresh branch. **Never** check out `main` (or any other long-lived shared
branch) directly into a worktree:

```bash
# Good — dedicated branch
git worktree add ../path -b feat/short-name origin/main

# Bad — locks main checkout, blocks operator/CI build
git worktree add ../path main
```

A worktree that holds `main` makes the canonical repo go `detached HEAD` (or
errors on `git switch main`) and breaks `cargo build --release` for operator
and tooling. The fix is `cd <worktree> && git switch -c <dedicated-branch>`,
but it is far cheaper to never take main in the first place.

This applies to fleet agents (lead/dev/reviewer) and to the MCP `repo` tool —
when calling `mcp__agend-terminal__repo action=checkout`, pass a non-main
`branch` argument (the daemon honours it verbatim and will not derive a fresh
branch for you).

## Daemon logging (#914)

Daemon tracing writes to a daily-rotating file via `tracing_appender::rolling`
at `$AGEND_HOME/daemon.log.<YYYY-MM-DD>`. Retention defaults:

- `AGEND_LOG_RETAIN_DAYS=N` (default 3) — `max_log_files` cap
- `AGEND_LOG_MAX_BYTES=2G` (or plain integer / `K`/`M`/`G` suffix) — hard
  directory-size backstop; hourly tick prunes oldest if total exceeds

Operator tail target stays `$AGEND_HOME/daemon.log` — on Unix it's a
symlink to the newest rotated file (re-pointed by the same hourly tick);
on Windows operators `glob daemon.log.*` (Developer Mode required for
symlink support).

**Accepted regressions vs pre-#914**:

- ANSI color codes no longer in the log (`with_ansi(false)`) — operator
  scripts grepping plain text now work without `sed 's/\x1b\[[0-9;]*m//g'`.
- systemd / `journalctl -u agend-terminal` no longer carries the full
  daemon trace; switch to `tail -F $AGEND_HOME/daemon.log`. (The unit
  template's stdout/stderr now only capture panics + migration-failure
  messages.)
- macOS launchd plist's `StandardOutPath` / `StandardErrorPath` route to
  `/dev/null`; same `tail` advice applies.

On first boot after the #914 binary lands, any pre-existing `daemon.log`
file (legacy unbounded) is renamed to `daemon.log.migration.<unix-epoch>`
and the rolling appender takes over a fresh path. Migration is
idempotent — re-running the old binary after the fix doesn't double-rotate.

## Daemon lifecycle files (#922)

A booted daemon publishes four files under `$AGEND_HOME/run/<pid>/`:

| File | Written by | Purpose | Tier (#879v4 spike vocabulary) |
|---|---|---|---|
| `.daemon` | `bootstrap::prepare` early | PID identity for `find_active_run_dir` liveness checks | daemon-pid-published |
| `api.cookie` | `auth_cookie::issue` after `.daemon` | 32-byte shared secret for cookie handshake on the daemon API socket | daemon-pid-published (auth-ready) |
| `api.port` | `api::serve` thread after `bind_loopback` | TCP loopback port for the JSON control API | daemon-api-ready |
| `.ready` | daemon main thread after agent spawn loop completes | Boot-completion signal: spawn loop done, agent count final for this boot | daemon-init-complete |

`.ready` exists ⟹ the daemon's agent spawn loop has finished and
`agend-terminal list` (or `/api/list`) will return the FINAL agent
set for this boot. NOT a guarantee that `count == fleet.size` — the
daemon's log-and-continue policy lets individual agents fail without
aborting the loop, so final count may be less.

**Lifecycle file single-signal policy** (per dev-2 catch in #922 dialectic):
`.ready` is the SINGLE boot-completion signal. Future sub-stage signals
MUST extend `.ready`'s content (JSON payload with per-subsystem ready
flags) rather than introduce new files. The four-file table above is
the entire surface — no fifth file.

### Race-condition distinction

Two timing races have been closed across recent PRs — they live on
DIFFERENT surfaces:

- **#908** closed the per-agent `.port`-file race: `spawn_and_register_agent`
  now blocks until the per-agent TUI thread completes `bind_loopback` +
  `write_port`. After the function returns, that agent's `.port` file
  is on disk.
- **#922** closes the API-level partial-results race: external probers
  seeing `api.port` cannot assume the registry is populated, because
  `api.port` is written by `api::serve` BEFORE the agent spawn loop
  runs (per #906 reorder). `.ready` is the daemon-init-complete signal
  external probers wait for.

### Stale-marker safety

A residual `.ready` from a crashed daemon's old `run/<pid>/` directory
will still exist on disk until cleanup. Bare `until [ -f .ready ]`
polling is INSUFFICIENT in crash-residue scenarios — it can return
true for a stale marker. Operator-correct idioms that combine
`.ready` with PID-liveness:

```bash
# Idiom A (recommended): use `agend-terminal doctor`
until agend-terminal doctor 2>&1 | grep -q "Active agents:"; do sleep 0.1; done

# Idiom B: glob `.ready` + verify the run_dir's `.daemon` PID is alive
until for d in "$AGEND_HOME"/run/*/; do
  [ -f "$d/.ready" ] && pid=$(cat "$d/.daemon" 2>/dev/null) && kill -0 "$pid" 2>/dev/null && exit 0
done; do sleep 0.1; done
```

The CI smoke harness in `.github/workflows/ci.yml` polls `.ready`
directly; it also checks `kill -0 $DAEMON_PID` each loop iteration
(daemon-died gate), which gives equivalent liveness coverage to Idiom B
because the smoke daemon's PID is captured at spawn time.

### Interaction with `runtime::list_agents_with_fallback` (#910)

`runtime::list_agents_with_fallback` (the canonical "enumerate live
agents" helper that #910 PR1 introduced) does NOT wait for `.ready`.
Callers that want a guarantee of complete enumeration should wait for
`.ready` first; bare callers during the boot window may legitimately
return a partial set as the spawn loop is in flight.

## Bootstrap instrumentation (#945 Phase 0)

`bootstrap::prepare` emits a `bootstrap-step` tracing line for every
instrumented step with `step=<name>` + `elapsed_ms=<n>` fields. Operators
can extract the cold-boot timing breakdown without instrumenting
externally:

```bash
# Top 5 slowest bootstrap steps from the most recent boot
grep "bootstrap-step" $AGEND_HOME/daemon.log \
  | sort -t= -k3 -n -r \
  | head -5

# Full ordered timeline of a single boot
grep "bootstrap-step" $AGEND_HOME/daemon.log
```

13+ steps are instrumented today (`load_fleet_yaml`,
`stop_managed_agents`, `prune_stale_worktrees`, `bind_loopback`,
`migrate_legacy_watch_filenames`, `start_telegram_init`, …). The Phase 0
audit found `telegram_init` accounted for 92.5% of cold-boot wall time
(~6.1 s of 6.6 s); Phase 1 backgrounded that step (see next subsection).

## Telegram init backgrounded (#945 Phase 1)

`bootstrap::telegram_init::init` now returns `None` immediately and
spawns a fire-and-forget thread to do the actual init (5–10 sequential
`bot.create_forum_topic` HTTP calls + fleet-binding resolve). Cold-boot
wall time drops from ~6.6 s to ~0.5 s. Implications:

- **`api.cookie` + `api.port` land within milliseconds.** External
  probers / `agend-terminal list` race fewer seconds before seeing a
  reachable daemon.
- **`active_channel()` returns `None` until background init completes.**
  Callers are all on the >10 s tick cadence, so the delayed channel is
  invisible to them in practice. If you write a new caller that
  requires the channel synchronously, query `active_channel()` in a
  poll loop with a 30 s ceiling.
- **Registry attachment uses a `PENDING_REGISTRY` bridge.**
  `bootstrap::prepare` (caller) publishes the agent registry via
  `crate::agent::set_pending_registry`; the background init thread
  reads it after `register_active_channel` and calls `attach_registry`.
  Bounded 30 s poll with a 100 ms cadence covers the rare race where
  the bg thread finishes before the caller publishes.
- **Failure surfaces via `tracing::error!`.** No panic, no aborted
  boot. The `topic_registry` orphan sweep self-heals on the next boot.
  Operator can `tail -F daemon.log | grep telegram_init` to spot
  recurring failures.

## State detection red anchor (#919 Phase A)

State-detection patterns marked `HIGH_FP` (high false-positive risk —
generic strings like `"Error"`, `"failed"`, etc.) now require a red SGR
escape (`\x1b[31m` family) to appear in the captured byte stream
within 200 bytes and 30 seconds of the match. The anchor closes the
class of false transitions where a backend echoed an `Error: ...`
string from a user prompt (no red color) and the daemon classified the
agent as failed.

`Backend::has_red_anchor()` (`src/backend.rs`) declares per-backend
whether red SGR is reliably emitted on real errors. When `false`, the
HIGH_FP gate **fails open** (pattern alone fires the transition) so
backends without consistent color signaling aren't silently broken.

Telemetry gate (Phase B, gated separately): the FP-rate sample is
exported as Phase A ships; Phase B will tighten enforcement once
operator telemetry confirms the gate is doing more good than harm.
Until then, `HIGH_FP` matches without the red anchor log a debug line
naming the pattern + the missing-anchor reason, useful for fixture
collection.

## Operator diagnostic recipes

A consolidated set of `grep` recipes for the most common
"is the daemon healthy?" / "where did time go?" questions. Each is
self-contained — copy, paste, run.

```bash
# Zombie debugging — #932 closed via #941 observability;
# verify a zombie isn't still attached to a stale $AGEND_HOME
grep "shutting down (signal received)" $AGEND_HOME/daemon.log
cat /proc/<zombie-pid>/environ | tr '\0' '\n' | grep AGEND_HOME

# Bootstrap timing — top 5 slowest steps from the most recent boot (#945 Phase 0)
grep "bootstrap-step" $AGEND_HOME/daemon.log \
  | sort -t= -k3 -n -r \
  | head -5

# Live thread state dump — useful when the daemon appears wedged (#941)
AGEND_DAEMON_THREAD_DUMP_SECS=60 ./agend-terminal start
# Subsequent dumps appear in daemon.log every 60 s with
# `thread-dump` line + per-thread state summary.

# CI-watch correlation — find every notification for a given branch (#946)
grep '"correlation_id":"owner/repo@branch"' $AGEND_HOME/inbox/*.jsonl

# Dispatch-idle correlation fallback (#947) — find a watchdog firing
# whose upstream had no correlation_id; the synthesized id has a
# `disp-<unix_micros>-<seq>` shape
grep '"correlation_id":"disp-' $AGEND_HOME/inbox/*.jsonl | head -5

# Boot sweep dry-run preview before flipping the destructive mode (#933)
AGEND_DAEMON_BOOT_SWEEP_DRY_RUN=1 AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS=14 \
  ./agend-terminal start
grep "boot-sweep" $AGEND_HOME/daemon.log
```

## Release

Tags matching `v*` trigger `.github/workflows/release.yml`, which builds 5
targets (macOS x64/arm64, Linux x64/arm64, Windows x64) and uploads tarballs
+ `SHA256SUMS` to the GitHub release.

Before tagging: confirm the latest `ci.yml` run on `main` is green.
