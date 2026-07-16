[繁體中文](DAEMON-LOCK-ORDERING.zh-TW.md)

# Daemon Lock Ordering

**Sprint 23 P0 deliverable** (per dev-reviewer-2 cross-vantage demand) —
explicit acquisition-order doc to prevent deadlocks under concurrent
supervisor tick + MCP handler load.

**Status**: ACTIVE — all daemon code paths must observe this ordering.
**Maintained**: alongside `tests/heartbeat_pair_atomicity_audit.rs`
invariant test (Sprint 23 P0 anti-bypass).
**Revalidated**: named lock and writer/reader anchors against
`main@1d83b423` (2026-07-16). The conceptual levels below start at Level 0;
`sync_audit`'s runtime tier numbers start at 1. Do not compare the numeric
labels without that offset.

**Scope** (Sprint 23 P0 r2 F4 clarification): locks acquired in the
daemon's runtime hot path (supervisor tick + MCP handler dispatch + agent
lifecycle). Startup-only locks (`identity::LOCK`,
`fleet_normalize::WARNED`), cleanup-only locks
(`worktree_cleanup::ENV_LOCK`), and test-fixture locks are out of scope —
their non-runtime nature avoids the concurrent-acquisition class this doc
addresses.

---

## Hierarchy (acquire in this order; release in reverse)

```
Level 0 (root):
  agent_registry            — global HashMap<InstanceId, AgentHandle>
                              (`crate::agent::AgentRegistry`)
  external_registry         — global HashMap<String, ExternalAgentHandle>
                              (`crate::agent::ExternalRegistry`)
                              NOTE (G3 M5): `register_external` acquires
                              external_registry THEN agent_registry (read).
                              Never acquire agent_registry first then
                              external_registry — deadlock risk.
  configs                   — daemon-only HashMap<String, AgentConfig>
                              (`src/daemon/mod.rs::AgentConfig`)

Level 1 (per-agent, accessed via root):
  agent_handle.core         — Mutex<AgentCore>
                              (vterm + state + health + subscribers)
  agent_handle.child        — Mutex<Box<dyn portable_pty::Child>>
  agent_handle.pty_writer   — Mutex<Box<dyn Write>>
  agent_handle.pty_master   — Mutex<Box<dyn MasterPty>>

Level 2 (storage / transactional):
  task_events_jsonl_lock    — file lock around `<home>/task_events.jsonl`
                              (`crate::task_events::append`); anti-bypass
                              invariant `tests/task_events_invariant.rs`
                              enforces single-writer (Sprint 24 P0 PR #236)
  decision_store_lock       — per-decision file lock beside
                              `<home>/decisions/<id>.json`
                              (`crate::decisions::with_decision_lock`)
  inbox_jsonl_lock          — per-agent flock around append/fsync or
                              rewrite of `<home>/inbox/<agent>.jsonl`
                              (`crate::inbox::storage::with_inbox_lock`)

Level 3 (leaf-level):
  heartbeat_pair (per-agent) — `Mutex<HeartbeatPair>`
                              (`crate::daemon::heartbeat_pair::pair_for`)
  heartbeat_pair_registry    — outer `Mutex<HashMap<String, Arc<…>>>`
                              (`crate::daemon::heartbeat_pair::registry`).
                              Brief-acquire-only inside `pair_for()`;
                              never held across pair-lock acquisitions.
  TelegramState              — `Arc<Mutex<TelegramState>>`
                              (`crate::channel::telegram::lock_state`)
  channel sink registry      — `Mutex<Vec<Arc<dyn UxEventSink>>>`
                              (`crate::channel::sink_registry`)
  thread census              — `Mutex<HashMap<&'static str, AtomicU32>>`
                              (`crate::thread_census::census`).
                              Sprint 26 PR-B counter-only registry; brief
                              acquire on register/Drop/snapshot, never held
                              during nested acquisitions.
```

## Hierarchy rules

1. **Top-down acquisition**: a thread holding a Level N lock may acquire
   Level N+1 or higher locks. NEVER acquire a lower-level lock while
   holding a higher-level lock.

2. **Drop before climbing**: if a thread holds a Level 1 lock (e.g. core)
   and needs a Level 0 lock (e.g. agent_registry), it MUST release the
   Level 1 lock first. Otherwise → deadlock with another thread acquiring
   them top-down.

3. **Leaf-level locks (Level 3) NEVER held while acquiring any other
   lock**: heartbeat_pair / TelegramState / sink_registry must always be
   the last acquired and first released. This rule is the strictest — even
   transient acquisition of another lock while holding a leaf-level lock
   is forbidden.

4. **No cross-instance lock chaining at Level 1+**: holding agent A's
   `core` lock while acquiring agent B's `core` is forbidden (not a
   common case but documented for completeness — future fleet-broadcast
   refactor must observe this). (Within-instance contention on the same
   Level 1 lock is standard Mutex queueing — first acquirer wins, second
   blocks; no deadlock risk because there's no cycle.)

---

## Why these rules prevent deadlock

(Sprint 23 P0 r2 F1 — explicit deadlock-prevention proof sketch per
dev-reviewer-2 cross-vantage demand.)

Deadlock requires a cycle in the lock-acquisition graph (thread A waits
on lock X held by thread B, thread B waits on lock Y held by thread A).
Rule 1 (top-down) forces every thread to acquire locks in the same
partial order Level 0 → Level 1 → Level 2 → Level 3 — eliminating
cross-level back-edges. Rule 3 (leaf never held during another
acquisition) collapses Level 3 to brief-acquire-immediate-release,
eliminating leaf locks from the acquisition-edge graph entirely. Rule 2
(drop before climbing) prevents accidental violations of Rule 1 by code
paths that need to re-enter root locks. Rule 4 (no cross-instance
Level 1) prevents intra-level cycles when two threads operate on
different agents. Composition: every thread's lock-acquisition trace is
a strictly-increasing total order over levels, with leaf locks acting
only as instantaneous-release sinks → no cycle possible → deadlock-free.

---

## Why heartbeat_pair is leaf-level (Sprint 23 P0 F6)

The `heartbeat_pair` lock began as the three timing fields below and now also
owns per-turn reply-routing/settlement fields. `snapshot_for` clones the whole
`HeartbeatPair` under one brief guard, preserving a consistent view across the
per-tick + MCP heartbeat/write race window identified in the Sprint 20 audit. Timing
fields:

- `heartbeat_at_ms: u64` — last MCP tool call timestamp (Sprint 23 P0 PR #235)
- `waiting_on_since_ms: Option<u64>` — when current `waiting_on` started (Sprint 23 P0 PR #235)
- `last_input_at_ms: u64` — last daemon→agent input delivery timestamp (Sprint 24 P1 PR #243)

The additional `reply_to_*`, mirror, pending-turn, and settled-group fields are
listed on `HeartbeatPair` in `src/daemon/heartbeat_pair.rs`; they follow the
same leaf-lock rule and are intentionally not duplicated here.

Per dev-reviewer-2 threat model synthesis, the lock-around-pair design is
preferred over `AtomicU64` per-field because correctness-corruption
(prompt-injection, capability bypass) is the actual fleet threat — atomic
exposes inconsistent-pair window.

Leaf-level placement means:

- **MCP heartbeat write site** (`src/mcp/handlers/mod.rs`, implicit
  heartbeat in `handle_tool`): acquire pair lock, update field, release lock, THEN
  `save_metadata` for crash-recovery persistence. Disk I/O happens
  outside the lock.

- **MCP `set_waiting_on` write site**
  (`src/mcp/handlers/instance_metadata.rs::handle_set_waiting_on`):
  acquire pair lock, update both fields, release lock, THEN
  `save_metadata_batch` for atomic disk write. Two-stage: in-memory
  pair-update first, disk persist second. Sprint 22 P2a `save_metadata_batch`
  helper (PR #233, my author) handles disk-side atomicity; pair lock
  handles in-memory atomicity.

- **Hang-detection read site**
  (`src/daemon/per_tick/hang_detection.rs::HangDetectionHandler::run`):
  acquire pair lock for pair snapshot, release lock immediately, then
  use the copied snapshot for `check_hang`. This acquisition occurs while the
  per-agent core lock is held, which is the permitted top-down Level 1 →
  Level 3 order. The pair guard is released inside `snapshot_for` before
  `check_hang` continues, so no leaf lock escapes the call.

If a future contributor needs to hold the pair lock while acquiring
another lock, they MUST first refactor to acquire the other lock first
(climbing the hierarchy), or restructure so the pair lock is acquired
after every other lock has been released.

---

## Anti-bypass invariant test

`tests/heartbeat_pair_atomicity_audit.rs` (Sprint 23 P0 deliverable)
enforces:

1. **Source-grep guard**: every `save_metadata` / `save_metadata_batch`
   call site that writes `last_heartbeat` OR `waiting_on_since` MUST be
   accompanied (within preceding lines) by a `heartbeat_pair::update_with`
   or `heartbeat_pair::pair_for(...).lock()` call. Pre-pair writes that
   skip the in-memory update are flagged.

2. **EXEMPTED_LEGACY_FILES** anti-growth contract: empty by intent. New
   entries are forbidden without explicit dispatch scope. The pattern came
   from a retired Sprint 22 outbound-path audit; the live enforcement is in
   `tests/heartbeat_pair_atomicity_audit.rs`.

---

## Operational notes

- **Crash recovery**: heartbeat_pair is an in-memory cache; on daemon
  restart, the pair starts empty (`heartbeat_at_ms == 0`). Supervisor
  falls back to `read_heartbeat_age` (disk read) until the next MCP
  heartbeat populates the pair. This is a graceful degradation — the
  race window only exists when both supervisor read AND MCP write occur
  during the daemon's first tick after restart, which is a tiny window.

- **Per-instance key**: pair lock is keyed by agent name. Two agents
  cannot deadlock on each other's pair locks (different keys, different
  Mutexes). Sprint 24 P0 task sweep daemon uses the same per-key pattern
  for `task_events.jsonl` so the leaf-level rule extends naturally.

- **Forward-compat with graceful-join (Sprint 25+)**: the
  `daemon::ticker::DaemonTicker` primitive (Sprint 23 P0 also)
  already stores JoinHandle for future graceful-join consumers. Pair
  lock is unaffected by the join refactor — Mutex<HeartbeatPair> is
  thread-affinity-free.

- **F1 heartbeat-spam cross-check (Sprint 24 P2 PR #249)**: the
  `check_hang` classifier cross-checks `heartbeat_at_ms` freshness
  against PTY silence. "Heartbeat fresh" means the agent recently called
  MCP tools (implicit heartbeat in the MCP dispatch chokepoint). "PTY silent"
  means no operator-visible output was produced. If heartbeat is fresh
  but PTY is silent past the hang threshold, the classifier overrides
  `IdleLong` → `Hung` — catching prompt-injected agents that suppress
  escalation by spam-calling MCP tools without generating real output.
  This cross-check reads `heartbeat_at_ms` from the pair snapshot
  (Level 3 leaf) acquired UNDER the core lock (Level 1 → Level 3
  top-down per Rule 1), with the pair lock acquired+released
  synchronously by `snapshot_for` so it's not held during subsequent
  `check_hang` execution (Rule 3).

---

## Related

- Sprint 20 Track B daemon audit (historical; retrieve it from repository
  history): §1 F6 (this race window) + F7 (the disk-side companion, fixed
  Sprint 22 P2a PR #233 via `save_metadata_batch`).
- Sprint 22 P2a: the now-retired outbound-path audit established the
  EXEMPTED_LEGACY_FILES anti-growth template retained by the live heartbeat
  atomicity audit.
- Sprint 21 PR #226: protocol §12.5 spawn site rationale (parallel
  doc-doc convention; this lock-ordering doc is the Sprint 23 analogue
  for shared-state primitives).
- Sprint 24 P0: PR #236 (`task_events.jsonl` storage substrate), PR #239
  (`tasks.json` retired to `.legacy_pre_v2` archive — Level 2 entry removed).
- Sprint 24 P1: PR #243 (daemon health classifier — `HeartbeatPair`
  extended with `last_input_at_ms` for `IdleLong` vs `Hung` discrimination).
- Sprint 24 P2: PR #249 (F1 heartbeat-spam cross-check — `heartbeat_at_ms`
  freshness vs PTY silence override).
