# agend-terminal — Claude working notes

## Rust workflow

Before committing any Rust change, **always** run:

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
```

CI runs these in the first two steps of `ci.yml`. Skipping them locally means
the next push fails and needs an extra "fix fmt / fix clippy" round trip.

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

## Release

Tags matching `v*` trigger `.github/workflows/release.yml`, which builds 5
targets (macOS x64/arm64, Linux x64/arm64, Windows x64) and uploads tarballs
+ `SHA256SUMS` to the GitHub release.

Before tagging: confirm the latest `ci.yml` run on `main` is green.
