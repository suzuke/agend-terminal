[繁體中文](RUNBOOK.zh-TW.md)

# Incident Runbook

Symptom-driven recovery guide. Every command here was run against a live
deployment before being written down. Set `$AGEND_HOME` to the directory the
daemon uses before running them. A fresh install normally uses `~/.agend`; if
only the legacy `~/.agend-terminal` exists, the daemon keeps using it.

**Find the log first.** Log files are date-stamped with the **UTC** date and
rotate daily — `app.<YYYY-MM-DD>.log` when you run the TUI (`agend-terminal
app`, the common deployment) and `daemon.<YYYY-MM-DD>.log` for a headless
`agend-terminal start`. The current day's file only appears after the first
write of that UTC day, so just after midnight UTC "today's" file may not
exist yet. The robust way to grab the newest:

```sh
ls -t "$AGEND_HOME"/app.*.log "$AGEND_HOME"/daemon.*.log 2>/dev/null | head -1
```

Two other files you will meet repeatedly:

- `event-log.jsonl` — one-line operator-visible events (the daemon's
  "things you should know" channel). `grep <kind>` it.
- `state-transitions.jsonl` — every agent state change with a timestamp and
  a screen snippet (see §2).

---

## 1. Daemon won't start / crash-loops

**Symptom**: `agend-terminal app` (or `start`) exits immediately, or agents
keep dying and respawning.

**Diagnose**

```sh
LOG=$(ls -t "$AGEND_HOME"/app.*.log "$AGEND_HOME"/daemon.*.log 2>/dev/null | head -1)
grep -E " ERROR " "$LOG" | tail -20
agend-terminal doctor          # home dir / .env / fleet.yaml / live agents, all checked
```

What the crash machinery looks like in the log: each agent crash logs
`crashed`, respawns are delayed with backoff, and after **5 total crashes**
the agent's health goes `Failed` — no further respawns, one notification.
For an orchestrator that means a line like:

```
self-orchestrator PERMANENTLY FAILED (respawn budget exhausted) — escalating terminal P0
```

Crash-budget state **persists across daemon restarts** (so a restart can't
be used to reset a respawn storm by accident). If persisting that state
itself fails (e.g. disk full) you'll see `escalation_persist: write FAILED`
at ERROR level plus one `escalation_persist_failed` event-log entry —
treat that as "fix the disk first".

**Recover**

```sh
agend-terminal stop            # graceful: suppresses crash handling during shutdown
agend-terminal app             # or: agend-terminal start (headless)
# service installs instead:
agend-terminal service status  # then restart through your init system
```

If one agent keeps crashing after a clean daemon restart, the problem is
that agent's backend/working dir, not the daemon: `agend-terminal attach
<agent>` to see its screen, then fix the underlying cause before
respawning (`agend-terminal kill <agent>` + restart).

---

## 2. An agent looks stuck / shows a weird state

**Symptom**: the badge says `awaiting_operator` / `hung` / `starting` but
the agent looks fine — or the agent really is wedged.

**Diagnose**

```sh
# What changed, when, and what the screen looked like at that moment:
grep '"agent":"<name>"' "$AGEND_HOME/state-transitions.jsonl" | tail -5
# Look at the live screen (read-only enough — detach by closing the viewer):
agend-terminal attach <name>
agend-terminal doctor
```

Each `state-transitions.jsonl` line is
`{"agent","from","to","ts","pty_snippet"}` — the snippet is the bottom of
the pane at transition time, which usually answers "why did it think that"
directly.

Known history: a whole class of **false `awaiting_operator` after a daemon
restart** was fixed in #2020/#2021 (idle opencode panes resuming a session,
and busy agents that skipped the clean ready-prompt). If you see that shape
on a version older than those fixes, upgrade; on a current version, treat
`awaiting_operator` as real and look at the captured pane.

**Recover**

- Agent truly wedged → `agend-terminal kill <name>`; the daemon respawns it
  (within the crash budget, §1).
- A worktree binding survived its agent (e.g. you deleted/recreated an
  instance and `bind_self` now refuses): first inspect
  `binding_state({instance:"<name>"})`. The worktree owner or its team
  orchestrator may then use
  `release_worktree({instance:"<name>", force:true, branch:"<exact-branch>"})`.
  Force release requires the exact branch and refuses opaque, mismatched,
  markerless, or ambiguous ownership instead of guessing.
  Before removing a dirty worktree, the daemon snapshots recoverable tracked,
  staged, and untracked WIP to a `refs/agend/recovery/...` ref and reports the
  recovery ref. If that snapshot fails, or dirt exists inside a nested
  submodule that the parent ref cannot preserve, release fails closed and
  leaves the directory in place. Never bypass a refusal or remove the
  directory manually; preserve/commit the reported WIP and retry through the
  same daemon operation.

---

## 3. Task board frozen / won't load

**Symptom**: `task` queries return errors or the board simply never
changes.

This is usually the **deliberate fail-closed gate** (#1992): the task event
log contains a record this daemon version doesn't understand (most often:
the log was written by a NEWER daemon — i.e. you downgraded). The daemon
keeps running but refuses to advance the board rather than guess.

**Diagnose**

```sh
LOG=$(ls -t "$AGEND_HOME"/app.*.log | head -1)
grep "FAIL-CLOSED" "$LOG" | tail -3
grep "task_replay_fail_closed" "$AGEND_HOME/event-log.jsonl"
```

The ERROR line spells out the contract:

```
task-board replay FAIL-CLOSED — the board will not advance until resolved
(upgrade the daemon to a version that understands this log, or quarantine
the offending record)
```

(The event-log entry fires **once per boot** so per-tick retries can't spam
you; the greppable ERROR repeats.)

**Recover**

- Future-version record → **upgrade the daemon**. This is protection, not
  a bug: see `docs/COMPATIBILITY.md` tier (b).
- Genuinely garbled lines (a torn write from a crash) are handled for you:
  at every startup the daemon quarantines non-JSON lines into
  `$AGEND_HOME/task_events.recovery/<timestamp>/` and keeps the good
  ones. Check that directory to see what was pulled out. Valid-JSON
  future-version records are deliberately NOT auto-dropped (they belong to
  a newer daemon — upgrading restores them).

---

## 4. A store file reset itself ("where did my X go?")

**Symptom**: some persisted state (schedules, runtime config, …) is
suddenly empty/default after a restart.

The store loader found corrupt JSON, **renamed the bad file aside**, and
started with defaults (#2017): the backup is right next to the original as

```
<store-file>.corrupt.<YYYYMMDDHHMMSS>
```

**Diagnose**

```sh
ls "$AGEND_HOME"/*.corrupt.* 2>/dev/null
LOG=$(ls -t "$AGEND_HOME"/app.*.log | head -1)
grep "store load: corrupt JSON" "$LOG"
grep "store_corrupt" "$AGEND_HOME/event-log.jsonl"   # once per boot per file
```

**Recover**

```sh
agend-terminal stop
cp "$AGEND_HOME/<store>.corrupt.<ts>" /tmp/inspect.json   # look at it; often a truncated tail
# fix the JSON (usually: delete the torn last record), then:
mv /tmp/inspect.json "$AGEND_HOME/<store>"
agend-terminal app
```

If you don't need the old contents, do nothing — the daemon already runs
with a fresh default and will overwrite on the next write. The backup stays
until you delete it.

---

## 5. Notifications not arriving (Telegram quiet, agent badge stuck)

**Symptom**: an agent shows pending notifications, or Telegram messages
stop arriving.

Deferred messages live in `$AGEND_HOME/notification-queue/` (one
`.jsonl` file per agent; line count = pending messages). They are held
on purpose while the agent is mid-generation or you are mid-keystroke,
and released by both the TUI loop and a daemon-side per-tick flusher
(headless deployments included), with anti-starvation caps of ~1s for
actionable messages and ~7s for ambient ones. Since #2029 a contended
queue is retried, never misreported as empty.

**Diagnose**

```sh
wc -l "$AGEND_HOME"/notification-queue/*.jsonl 2>/dev/null
LOG=$(ls -t "$AGEND_HOME"/app.*.log | head -1)
grep "telegram notify failed" "$LOG" | tail -3   # network/token class
grep "requeue FAILED" "$LOG"                     # disk class — a queued message was LOST
```

**Recover**

- `telegram notify failed` → it's the network or the bot token. Verify the
  token env (`AGEND_TELEGRAM_BOT_TOKEN` in `$AGEND_HOME/.env`), the
  bot's group membership/admin rights, and connectivity. The daemon keeps
  retrying; nothing to clean up locally.
- Queue files growing without bound while agents are idle → attach to the
  agent (§2) to see why the flusher thinks it's busy, and check the
  `#1944-draftgate-decision` lines in the log (they record every
  hold/release decision).
- A persistent `requeue FAILED` is disk trouble — fix that first; the
  affected line is logged with the head of the lost text.

---

## 6. Upgrading / downgrading safely

Read `docs/COMPATIBILITY.md` first — it defines the three on-disk tiers:
(a) hand-edited public files like `fleet.yaml` (additive-only, carries
`schema_version`), (b) daemon-internal persisted state (versioned;
newer-than-supported records are warned about or fail-closed), (c)
regenerable files (no commitment).

- **Upgrade**: stop, install, start. State written by any older same-major
  version is read.
- **Downgrade**: expect tier-(b) friction — a task event log written by the
  newer version **fail-closes the board on purpose** (§3). That is the
  protection working, not a bug; going back to the newer version restores
  everything. `fleet.yaml` from a newer version loads with a warning
  (unknown fields are ignored — check the WARN before trusting behavior).
- After upgrading a **service install**, re-run `agend-terminal service
  install` once so the unit/plist carries current settings, then restart
  the service (see `docs/RELEASING.md` and the service docs for details).

---

## 7. GitHub Actions is degraded

Activate this procedure only when the same repository has at least two PRs
with no workflow run ten minutes after push **and** GitHub Status reports
Actions as degraded or unavailable.

1. Freeze merges for affected PRs and record each branch plus immutable head
   SHA. Local green never authorizes `--admin`, `--force`, or another gate
   bypass.
2. Run local diagnostics in each PR worktree:

   ```bash
   cargo test --all
   cargo clippy --all-targets -- -D warnings
   cargo fmt --check
   ```

3. Post the results as diagnostic evidence and state that hosted
   cross-platform CI is still pending. Keep the PR open.
4. After recovery, ensure Actions runs against the same PR head, re-arm the
   branch `ci` watch if needed, and independently run `gh pr checks <PR#>`.
5. Merge only after that command exits 0 and every required review gate also
   holds. A later `main` run is not backfilled evidence for the PR; a changed
   PR head invalidates the old review and diagnostics.

The GitLab mirror may help diagnose a GitHub outage, but it does not replace
the required checks configured on the GitHub PR head.

---

## 8. Start a context-isolated Claude instance

Claude can inherit instructions or memory from three places:

| Source | Scope | Default |
|---|---|---|
| `~/.claude/CLAUDE.md` | every local Claude session | loaded |
| `~/.claude/projects/<pwd-slug>/memory/` | working-directory memory | loaded when present |
| `<workspace>/CLAUDE.md` and `<workspace>/.claude/CLAUDE.md` | project | loaded when present |

AgEnD already gives each instance a distinct workspace/worktree, so
working-directory memory does not cross instance names. To disable both loading
and writing auto-memory for one new instance, pass the supported per-instance
environment map:

```text
create_instance(
  name="clean-agent",
  backend="claude",
  env={"CLAUDE_CODE_DISABLE_AUTO_MEMORY":"1"}
)
```

To exclude the global `CLAUDE.md` as well, create
`$AGEND_HOME/workspace/clean-agent/.claude/settings.json` before the first
launch:

```json
{
  "autoMemoryEnabled": false,
  "claudeMdExcludes": ["~/.claude/CLAUDE.md"]
}
```

For security-sensitive default-behavior testing only, a throwaway per-instance
`HOME` gives the strongest isolation, but it also removes access to existing
Claude authentication, MCP settings, and themes. None of these options removes
the fleet protocol injected by the daemon, disables workspace MCP servers, or
deletes memory already stored on disk.
