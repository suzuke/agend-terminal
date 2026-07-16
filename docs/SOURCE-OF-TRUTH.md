[繁體中文](SOURCE-OF-TRUTH.zh-TW.md)

# Source-of-Truth Matrix

**Status**: ACTIVE — engineering norm. New state, new storage, or a new
reader of existing state must be classified here before merge.

**Origin**: formalizes `workspace/fugu-0acdd8/agend-terminal-solutions.md`
§3.2–§3.6 (source-of-truth dispersion). Motivation: dispersed truth is an
empirical failure class — the 2026-07 branch pile-up root cause was a *stale
field nobody knew was dead* (`AgentConfig.worktree_source`, since removed;
task `…67777-3`); the
post-#994 `topics.json` single-source rule and the `binding.json` truth-source
fix were each established case-by-case. This doc systematizes them.

**Revalidated**: named stores, writers, and readers below were checked against
`main@1d83b423` (2026-07-16). Function/type names are the stable anchors;
`path:line` suffixes are navigation hints and may move as files are split. When
you touch a listed entry point, update the anchor and its line hint here.

---

## The three data classes

Every piece of state belongs to exactly one class.

| Class | Definition | Rule |
|---|---|---|
| **Source** | The sole writable truth. Mutations happen here. | Only this store may be treated as authoritative. It may be a file or in-memory. |
| **Projection / Cache** | Derived from a Source; rebuildable; may be stale. | Must never be the basis for an *irreversible* mutation. A reader may use it, but see the fail-open rule below. |
| **Side-effect** | Emitted outward (PTY line, Telegram/Discord message, CI notification). | Once sent, it is gone. Never read it back as state truth. |

**Fail-open rule for Projection readers** (see `snapshot` below): a decider
*may* read a projection, but if the projection is missing or stale the decider
must degrade to a conservative, idempotent fallback — never a data-destroying
or otherwise irreversible action.

---

## Summary matrix

| State | Authoritative source | Source class | Non-source copies |
|---|---|---|---|
| Instance declarative config | `fleet.yaml` | Source (file) | in-mem `FLEET_CACHE` = projection |
| Teams | `fleet.yaml` `teams:` block | Source (file) | runtime team view = projection |
| Live agent process | in-memory `AgentRegistry` | Source (in-mem) | `.port` file = discovery index (projection) |
| Daemon discovery | live daemon process + run dir | Source (process) | `api.port`, `api.cookie` = projection |
| Task state | task event log (`task_events.jsonl`) | Source (append-only file) | rendered task list = projection |
| Inbox state | inbox storage (`inbox/<name>.jsonl`) | Source (append-only file) | PTY-injected message = side-effect |
| Decision state | decision store (`decisions/*.json`) | Source (file) | channel notification = side-effect |
| Worktree lease/binding | `binding.json` | Source (file) | git worktree dir + `.agend-managed` = materialized identity evidence; removed `AgentConfig.worktree_source` is historical only |
| Agent runtime state | in-memory `StateTracker` | Source (in-mem) | `snapshot.json` = fail-open projection |
| Channel / topic binding | `topics.json` | Source (file) | `fleet.yaml` `topic_id` = fallback |
| CI watch | `ci-watches/<hash>.json` sidecar | Source (file) | CI notification = side-effect |
| pr_state | *(none — cache)* | Projection / Cache | GitHub is the terminal truth |

---

## Per-state detail (read/write entry points)

### Instance declarative config — Source: `fleet.yaml`
- **Write**: `src/fleet/persist.rs:19` `mutate_fleet_yaml()` (acquires the
  `acquire_lock` guard, then calls `atomic_write_yaml`); e.g.
  `add_instances_to_yaml:67`.
- **Read**: `src/fleet/mod.rs:665` `FleetConfig::load()` → `load_arc:632` →
  `load_uncached:674`.
- **Projection**: `FLEET_CACHE` mtime/size cache (`src/fleet/mod.rs:632-656`),
  invalidated on write via `invalidate_cache()` (`src/fleet/persist.rs:10`).
- ⚠ **Write-entry not fully converged**: `src/quickstart.rs:811` writes
  `fleet.yaml` with a raw `std::fs::write`, bypassing the lock + atomic path.
  This is confined to the interactive quickstart overwrite-confirm branch
  (`:741-763`), not a runtime path. Tracked here as an exception, not a
  second truth source.

### Teams — Source: `fleet.yaml` `teams:` block
`src/teams.rs:1-10` states "operator-edited fleet.yaml is the source of truth."
- **Write**: `src/fleet/persist.rs:327` `add_team_to_yaml()` / `:370`
  `remove_team_from_yaml()` / `:387` `update_team_in_yaml()`, called from
  `src/teams.rs:222` `create()`.
- **Read**: `src/teams.rs:61` `load_fleet()` → `FleetConfig::load`.
- **Projection**: `src/teams.rs:117` `project_team()`. `list()`
  (`src/teams.rs:399`) cross-references the live registry
  (`src/runtime.rs:28` `list_live_agents`) to mark `stale_members` — it never
  mutates `fleet.yaml`, confirming the registry is an independent live truth.

### Live agent process — Source: in-memory `AgentRegistry`
`src/agent/mod.rs:129` `Arc<Mutex<HashMap<InstanceId, AgentHandle>>>`. Not
persisted.
- **Write**: insert at `src/daemon/mod.rs:1758-1763` (spawn); delete via
  `lifecycle::delete_transaction`.
- **Read**: `agent::lock_registry` (e.g. `src/daemon/mod.rs:1775`).
- **Projection**: `.port` file. `src/ipc.rs:47` `write_port` (atomic), from
  `src/daemon/tui_bridge.rs:61` (per-agent, written *after* the registry
  insert). Read: `src/ipc.rs:49` `read_port`. `src/ipc.rs:1-4` self-describes
  the port files as a discovery index. Rollback proof: on prep failure
  `src/daemon/mod.rs:1786-1791` runs `delete_transaction`, rolling back the
  registry and clearing the port — registry is primary, port is projection.

### Daemon discovery — Source: live daemon process + run dir
- **Run dir**: `src/daemon/mod.rs:292` `run_dir()` / `:299`
  `run_dir_for_pid()` (`home/run/<pid>`, PID is the key). Identity stamp
  written by `:440` `write_daemon_id()` (atomic `:451`). Read by `:323`
  `find_active_run_dir()` (scans, checks PID liveness + `.daemon` identity,
  `:338-370`).
- **api.port** (projection): `src/api/mod.rs:241` → `crate::ipc::write_port`;
  read `src/ipc.rs:84` `connect_run_dir_api`.
- **api.cookie** (projection): `src/auth_cookie.rs:29` `issue()` (0600,
  tmp+rename), from `src/daemon/mod.rs:546`; read `src/auth_cookie.rs:70`
  `read_cookie`. File name `api.cookie` (`src/auth_cookie.rs:23`).

### Task state — Source: task event log
`src/task_events.rs:3-4` — "Source-of-truth storage for task board state."
- **Write**: `src/task_events.rs:1062` `append`; `:1093` `append_batch_at`
  (actual disk write).
- **Read / projection**: `src/tasks/mod.rs:383-388` `list_all_at` →
  `task_events::replay_at` (`src/task_events.rs:1636`). The rendered task list
  is a replay projection.
- **Anti-bypass invariant** (enforced): `tests/task_events_invariant.rs:5-7`
  — only `src/task_events.rs` may reference the `task_events.jsonl`
  string; every other production caller must go through the `append` /
  `append_batch` public API. Many modules *do* call `append*` directly
  (e.g. `src/schedules.rs:931`, `src/daemon/idle_watchdog.rs:976`,
  `src/api/handlers/messaging.rs:206`) — that is allowed; direct *file* access
  is not.

### Inbox state — Source: inbox storage
`src/inbox/mod.rs:1-3` — append-only JSONL, one file per agent
(`{home}/inbox/{name}.jsonl`).
- **Write**: `src/inbox/storage.rs:170` `enqueue` (flock + append + fsync,
  `:178-191`).
- **Read**: `src/inbox/storage.rs:421` `drain` / `:662` `ack` (storage *is*
  the truth; no separate projection).
- **Side-effect**: `src/daemon/delivery_worker.rs:116-128` `dispatch()`
  handles a `PtyWake` job → `src/inbox/notify.rs:675-696`
  `inject_with_submit_direct` writes the line into the agent's PTY. The
  injected line is a delivery side-effect, not truth.
- Note: no anti-bypass invariant test exists for inbox (unlike task_events);
  `enqueue` is called directly from many modules.

### Decision state — Source: decision store
`src/decisions.rs:1` — CRUD over JSON files in `{home}/decisions/`.
- **Write**: `src/decisions.rs:170` `save` (via `store::save_atomic`), from
  `:191` post / `:416` update / `:505` answer.
- **Read**: `src/decisions.rs:127` `load_all`, `:363` `list_all`, `:395`
  `list`.
- **Side-effect**: `src/mcp/handlers/task.rs:13-24` `handle_post_decision`
  calls `decisions::post`, then emits
  `UxEvent::Fleet(FleetEvent::PostDecision{…})`
  (`src/channel/ux_event.rs:113,240-249`) → Telegram/Discord notification.
- Boundary exception: GC/archival in `src/daemon/retention/decisions.rs:56-66`
  moves files with `std::fs::rename` directly, but under
  `decisions::with_decision_lock` (`src/decisions.rs:180`).

### Worktree lease/binding — Source: `binding.json`
Daemon-only writer; the `agend-git` shim and hooks are read-only consumers
(`src/binding.rs:1-4`).
- **Write**: `src/binding.rs:266-390` `bind_full` (writes `:373-375`); clear
  via `unbind` `src/binding.rs:566-596` (removes `:586`).
- **Read**: `src/binding.rs:720-743` `read` (checks in-memory index first,
  then disk).
- **Materialized state (not source)**: the git worktree directory itself,
  created at `src/worktree.rs:81`. `src/worktree.rs:5-10,48` — the canonical
  layout means "production code reads `binding.source_repo` directly"
  (`binding.rs:371`).
- **Destructive release admission**: `repo release` snapshots the live binding
  and parses `.agend-managed`, then requires caller ownership plus exact
  agent/branch/source/path identity before delegating to
  `worktree_pool::release_full_exact` (`src/mcp/handlers/ci/release.rs`). A
  missing, corrupt, changed, or stale marker/binding fails closed; dirty WIP is
  preserved by the canonical release path. The marker is evidence that the
  materialized directory belongs to the lease, not an independent truth store.

### Agent runtime state — Source: in-memory `StateTracker`
`src/state/mod.rs:241` — per-agent state held under the agent core lock.
- **Projection write**: `src/daemon/per_tick/snapshot.rs:29-90`
  `SnapshotRotationHandler::run` reads `handle.core.lock()` (`:40-63`), builds
  an `AgentSnapshot`, and calls `crate::snapshot::save` (`:87`; disk write
  `src/snapshot.rs:48-57`).

### snapshot — Projection (fail-open)
`snapshot.json` is a read-optimized, file-based projection of `StateTracker`,
overwritten every tick for lock-free reads
(`src/daemon/per_tick/snapshot.rs`). It **is** read by deciders, so the
fail-open rule is mandatory. The production read surface is pinned by
`tests/snapshot_failopen_invariant.rs`; its eight current readers are:

| Decider | Read | Fallback on missing/stale snapshot |
|---|---|---|
| dispatch idle | `src/daemon/dispatch_idle/mod.rs` `snapshot::load` | `target_is_working` false → still fires a reversible nudge |
| inbox inject/deferred drain | `src/inbox/notify.rs` `agent_state_of` / `agent_is_busy` | missing → not busy → deliver the already-authoritative inbox payload now (timing changes only) |
| handoff timeout | `src/daemon/handoff_timeout_watchdog.rs` `agent_is_busy` | missing → not busy → reversible re-nudge |
| reply ledger | `src/reply_ledger.rs` `agent_is_busy` | missing → `emit_warn` + `NudgeAgent` — never an irreversible delete |
| stale-delivery reclaim | `src/inbox/storage.rs` `agent_is_busy` | missing → not busy → bounded reclaim/redelivery timing; no row is invented or dropped |
| daemon startup | `src/daemon/mod.rs` `snapshot::load` | missing → omit a diagnostic log only |
| status API | `src/api/handlers/query.rs` `snapshot::load` | missing → read-only empty status response |
| bug report | `src/bugreport.rs` `snapshot::load` | missing → omit the read-only snapshot section |

Field-level base: `src/snapshot.rs:21-38` (`#[serde(default)]`) +
`src/snapshot.rs:44-46` `default_silent_secs() → i64::MAX` — a missing field
reads as "very quiet," not "busy," steering every decider onto the
continue-action (never silently-swallow) path. The invariant above also blocks
an unreviewed reader from joining this surface. No snapshot reader performs an
irreversible mutation (worktree/branch deletion, etc.).

### Channel / topic binding — Source: `topics.json`
`src/bootstrap/doctor_topics.rs:10` — "topics.json is the single source of
truth." `src/channel/telegram/inbound.rs:142` — "topics.json is the canonical
source for topic_id → instance mapping."
- **Store**: `src/channel/telegram/topic_registry.rs:15-17`
  (`home.join("topics.json")`).
- **Write**: `topic_registry.rs:42-60` `register_topic` (flock read-modify-
  write) via `create_topic_for_instance:111`.
- **Read**: `src/channel/telegram/inbound.rs:130-153` `resolve_topic`;
  `src/fleet/resolve.rs:159-163`.
- **Fallback (not source)**: `fleet.yaml` `topic_id` (`src/fleet/mod.rs:456`,
  `:824`). Read on *every* resolve via `.or(inst.topic_id)`
  (`src/fleet/resolve.rs:159-163`) — not bootstrap-only — but only effective
  when `topics.json` has no entry for that instance.
- **Governance field**: `topic_binding_mode` (#2606, `src/fleet/mod.rs:512`,
  `:895`; surfaced by `list_instances`
  `src/mcp/handlers/instance_queries.rs:49-55`; gate at
  `topic_registry.rs:161`) decides *whether* to bind — it does not compete
  with `topic_id` for authority.

### CI watch — Source: `ci-watches/<hash>.json` sidecar
- **Store**: `src/daemon/ci_watch/registry.rs:4-6` (`home/ci-watches/`).
- **Write**: `src/mcp/handlers/ci/watch.rs:7` `handle_watch_ci`
  (`atomic_write` `:188`); unwatch `:251` / `:322` / `:360`.
- **Read**: `src/daemon/ci_watch/poller.rs:468`
  `check_ci_watches_with_provider` (`read_dir:473`), driven per-tick by
  `src/daemon/per_tick/ci_watch_poll.rs:28`.
- **Side-effect**: `src/daemon/ci_watch/poller.rs:1983` `deliver_ci_watch`
  (notification).

### pr_state — Projection / Cache (no local truth source)
A rebuildable cache of PR verdict/CI state; GitHub is the terminal truth.
- **Store**: `src/daemon/pr_state/mod.rs:458-460` (`home/pr-state/*.json`,
  keyed by repo+branch, not PR number, `:484-488`). Rebuildable
  (`src/daemon/pr_state/scanner.rs:14-19`).
- **Write**: `src/daemon/pr_state/mod.rs:917` `record_verdict` (from
  `src/api/handlers/messaging.rs:480,489,497`); `record_ci_result:836` (from
  `src/daemon/ci_watch/poller.rs:2160`).
- **Read**: `src/daemon/pr_state/mod.rs:492` `load`; `:528` `with_pr_state`
  (production). Consumer: `src/daemon/handoff_timeout_watchdog.rs:52-61`.

---

## Historical cases (why this doc exists)

Each of these was a real incident where a non-source copy was mistaken for
truth, or a source field silently died. They are the empirical basis for the
matrix.

### 1. `topics.json` vs `fleet.yaml` `topic_id` (#2598)
Both once looked like the topic→instance truth. #2598 (`a0bf79e6`) settled it:
`topics.json` is authoritative; `fleet.yaml` `topic_id` is a best-effort
mirror. In `bind_topic_for_instance` (`src/channel/telegram/topic_registry.rs:172-186`)
`topics.json` is written **first**, and a failed `fleet.yaml` mirror write only
warns (`:182`) — it does not block the `Bound` result. **Lesson**: a mirror
that can fail without failing the operation is a projection, not a source.

### 2. `binding.json` vs removed `AgentConfig.worktree_source` (task `…67777-3`)
The 2026-07 branch pile-up traced to `AgentConfig.worktree_source`, a spawn-time
cache derived from the legacy `{repo}/.worktrees/{name}` layout. It stayed empty
for the canonical `$AGEND_HOME/worktrees/…` layout, so the registry-derived repo
set was empty and cleanup silently did nothing.

This is now a **resolved historical case**, not a live-field warning:
`AgentConfig.worktree_source` and `source_repo_of` are gone. Runtime cleanup
discovers repositories from live signed `binding.json` records via
`binding::bound_source_repos`; `WorktreeRegistrySweepHandler` separately passes
resolved fleet working directories to `worktree_cleanup::sweep_from_registry`
(`src/daemon/per_tick/worktree_registry_sweep.rs`). The real source-repo truth
remains `binding.json.source_repo`. **Lesson**: remove a dead projection after
convergence; leaving it in the type makes future readers mistake it for truth.

### 3. snapshot fail-open (solutions.md §3.4)
`snapshot.json` is not a useless cache — deciders read it (see the snapshot
section). The correct rule is not "never decide on snapshot" but "a decision
that reads snapshot must fail-open, be idempotent, and never cause irreversible
damage from a stale/missing snapshot." All production readers are enumerated
and role-reviewed by `tests/snapshot_failopen_invariant.rs`. **Lesson**: a
projection that feeds deciders is allowed, but only if every reader degrades
safely.

### 4. pr_state `record_verdict` (#2603)
`src/daemon/pr_state/mod.rs:917` `record_verdict` is the sole verdict writer of
the pr_state cache. #2603 (`4d13b2f3`) made the handoff watchdog
(`src/daemon/handoff_timeout_watchdog.rs:44-61`) read the branch's *cached*
pr_state snapshot — "independent of ci_watch … no GitHub call." That is safe
precisely because pr_state is a projection: a stale read at worst delays a
handoff nudge; the terminal merge/CI truth still lives on GitHub and is
reconciled by the `gh_poll` path. **Lesson**: reading a cache to avoid a
network call is fine when the cache is explicitly a projection and the reader
tolerates staleness.

---

## Write-entry discipline

solutions.md §3.5 proposes that only the owning module may write each core
state file. This is **partially enforced today**:
- **Enforced**: task event log — `tests/task_events_invariant.rs` is a live
  anti-bypass test.
- **Not yet enforced**: inbox and decisions have no equivalent invariant test;
  their `enqueue` / `save` APIs are the intended entry but are not
  test-guarded against direct file access.
- **Known exception**: `src/quickstart.rs:811` writes `fleet.yaml` directly
  (interactive overwrite branch only).

`agend-git` shim (§3.6): a separate binary that cannot link daemon internal
APIs. It is a **read-only** consumer of `binding.json` and protected refs and
does not go through the daemon public API; shared logic is via source include
or a future `agend-core` crate. The "everything must go through a domain API"
rule explicitly exempts the shim.

---

## Maintaining this doc

- Adding state or a new reader of existing state? Add/annotate its row before
  merge, with a `path:line`.
- Changing a listed entry point? Update its line reference here in the same PR.
- Found two stores each claiming to be the truth for one state? That is a
  **bug**, not a documentation gap — stop and report it.
