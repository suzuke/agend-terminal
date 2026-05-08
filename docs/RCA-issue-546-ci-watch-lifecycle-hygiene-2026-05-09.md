# RCA — issue #546 ci_watch lifecycle hygiene cluster

**Date**: 2026-05-09
**Sprint**: 57 Wave 1 Track A (Phase A — RCA, Path B doc-only)
**Author**: dev
**Reviewer slot**: Tier-1 codex single primary
**Phase B IMPL prereq**: this doc — gates dispatch of Phase B IMPL track(s)
**Source of truth**: `8725118` (post-Sprint-56 closeout main HEAD)

---

## TL;DR

Four lifecycle items in the ci_watch surface (one of them on the parallel
notification-dedup surface that shares the same "supervisor in-memory state /
on-disk artifact" axis). All four exist on `main` today. Three are **structural
gaps** with concrete fix shapes ready for Phase B IMPL dispatch; one (Item 2)
is a **partial-coverage gap** where the existing implementation is correct for
the auto-installed watch but does not enumerate ad-hoc watches added by the
agent post-bind. Item 4 (worktree physical placement) is explicitly **out of
scope** per lead's dispatch — Wave 4 candidate.

| # | Item | Severity | Fix-shape complexity |
|---|------|----------|---------------------|
| 1 | ci_watch TTL not GC'd eagerly | MEDIUM | Low — GC tick OR on-startup cleanup pass |
| 2 | release_worktree only unsubscribes binding-branch | MEDIUM | Low — enumerate-by-agent helper |
| 3 | ci_watch action bypasses E4.5 protected-ref gate | MEDIUM-HIGH | Low — mirror `worktree_pool::lease` check |
| 5 | Dedup ledger lost across daemon restart | MEDIUM | Medium — persistence design decision required |

---

## Item 1 — ci_watch TTL not enforced eagerly

### Status quo

ci_watch entries are persisted as JSON files under `$AGEND_HOME/ci-watches/`.
The schema (created by `handle_watch_ci` at `src/mcp/handlers/ci/mod.rs:220-248`)
includes two TTL-relevant fields:

- `expires_at` — absolute deadline, set to `now + WATCH_TTL_HOURS` at create
  (`ci/mod.rs:245`).
- `last_terminal_seen_at` — timestamp of the most recent terminal CI verdict
  (`success` / `failure` / `cancelled`).

Enforcement is **lazy / read-side / poll-driven** at `src/daemon/ci_watch.rs`:

- Line 1308-1315: `if expires_at < now → remove watch` — checked inside the
  per-tick poll loop.
- Line 1319-1325: `if (now - last_terminal_seen_at) > 72h → remove watch` —
  inactivity TTL, also checked inside the per-tick poll loop.

### Gap shape

The poll loop only iterates watches that are **currently active**: the loop
body opens each watch file, makes the GitHub API call, then applies expiry on
the response. **A watch that is never polled — because no GitHub API ticket is
issued — never has its TTL evaluated.**

Concrete failure modes:

1. **Branch deleted upstream**: subsequent polls 404 → `last_terminal_seen_at`
   never advances → watch eventually trips inactivity TTL. OK.
2. **Agent released without unsubscribe (see Item 2)**: subscriber array is
   non-empty but the agent is gone. Watch keeps polling → fine for TTL, but
   wastes API budget.
3. **Daemon restart immediately followed by branch-delete**: watch file on
   disk has expired `expires_at`, but no poll tick has run yet because the
   branch is now 404. The file persists indefinitely.
4. **No audit-log event** is emitted when a watch expires (the lazy `remove`
   at ci_watch.rs:1310 has no `event_log::log` call alongside it). Operator
   has no signal of cleanup activity.

### Recommended fix shape (Phase B IMPL)

Two-step:

1. **On-startup sweep**: at `daemon::ci_watch::spawn` entry, before the poll
   loop begins, walk `$AGEND_HOME/ci-watches/*.json`, parse `expires_at`, and
   remove any file whose `expires_at < now`. Idempotent; re-runs on every
   daemon start.
2. **Per-tick eager scan**: lift the TTL check from the per-watch poll body
   into a separate "GC pass" at the top of each tick — check ALL watch files
   for `expires_at < now`, not just the ones being polled this round.

Add `event_log::log(home, "ci_watch_expired", &watch.repo, &reason)` next to
each removal so operators can trace lifecycle events.

### Tests gap

`src/daemon/ci_watch.rs::tests` covers:

- `test_watch_expires_after_ttl_inactivity` (line 2492)
- `test_watch_expires_at_absolute` (line 2552)

Missing: **no test for "stale file on disk after daemon restart"**. Phase B
IMPL needs an integration test that writes an expired watch file directly,
spawns/restarts the supervisor, asserts the file is gone post-startup-sweep.

---

## Item 2 — release_worktree partial-coverage unsubscribe

### Status quo

`handle_release_worktree` (`src/mcp/handlers/worktree.rs:134`) calls
`worktree_pool::release_full(home, agent)` (line 146). The release path
(`src/worktree_pool.rs:249-275`) derives `(released_repo, released_branch)`
from the agent's `binding.json` BEFORE unbinding, then calls
`unsubscribe_ci_watches_for_release(home, agent, &repo, &branch)` (line 274).

`unsubscribe_ci_watches_for_release` (worktree_pool.rs:298-349):
- Scans `$AGEND_HOME/ci-watches/*.json`.
- For watches where `repo == released_repo && branch == released_branch`,
  removes `agent` from the `subscribers` array.
- Empty array → `remove_file`. Non-empty → atomic-rewrite the watch file.

The symmetric subscribe path is `bind_self → dispatch_auto_bind_lease →
handle_watch_ci` (`dispatch_hook/mod.rs:119`).

### Gap shape

The unsubscribe is **scoped to exactly ONE (repo, branch) pair** — the
agent's binding-branch. **It does not enumerate any other watches the agent
subscribed to via direct `ci action=watch` calls.**

Empirical evidence from this very session (2026-05-08 → 2026-05-09):
- Bound to `sprint56-track-i-phase2c-issue-531-hard-removal`.
- During Sprint 56 closeout, called `mcp__agend-terminal__ci action=watch
  repo=suzuke/agend-terminal branch=main` to follow the post-merge CI
  on the merge commit (`8725118`).
- Called `release_worktree` post-closeout.
- The auto-installed watch on `sprint56-track-i-phase2c-…` was correctly
  removed by the EC7 path. **The ad-hoc watch on `main` was NOT cleaned**
  — the unsubscribe loop's `branch == "sprint56-track-i-phase2c-…"` predicate
  rejects the `main` entry.

Secondary gap: the comment at worktree_pool.rs:270-272 documents that when
`released_repo` cannot be derived (non-GitHub remote / no origin), the entire
unsubscribe is a no-op. This is a documented choice ("leaking a stale
subscription beats cross-repo unsubscribe") but combines with the primary gap
to produce **two leak vectors** on release.

### Recommended fix shape (Phase B IMPL)

Replace `unsubscribe_ci_watches_for_release(home, agent, repo, branch)` with
`unsubscribe_all_ci_watches_for_agent(home, agent)`:

- Walk `$AGEND_HOME/ci-watches/*.json`.
- For EVERY watch, check if `agent ∈ subscribers`. If yes, remove.
- Apply same empty-array → delete-file semantics.
- Continue logging removals per-watch via `tracing::info!` (existing pattern
  at worktree_pool.rs:332-348).

The repo+branch scoping in the current EC7 implementation was added per
reviewer feedback (`Sprint 55 P0-B EC7 r1: repo+branch exact match per
reviewer m-99`) to avoid cross-repo bleed. That risk does not apply to
agent-keyed unsubscribe — `agent` is unique within the fleet, so removing
its name from any watch's subscriber list is always correct on release.

### Tests gap

`bind_self_then_release_worktree_clean_state` (worktree.rs:511) covers the
auto-watch + auto-unsubscribe round-trip. Missing: **integration test for
"agent added a second watch via direct ci action=watch, then
release_worktree leaves the second watch subscribed"**. Phase B IMPL needs to
add this with the new `unsubscribe_all_ci_watches_for_agent` helper.

---

## Item 3 — ci_watch on `main` bypasses E4.5

### Status quo

E4.5 (the "no protected-ref leases" invariant) is enforced in
**`worktree_pool::lease`** at `src/worktree_pool.rs:21-32`:

```rust
if branch == "main" || branch == "master" {
    return Err(anyhow!("E4.5 violation: cannot lease worktree for protected branch"));
}
```

`bind_self` correctly routes through this gate via
`handle_bind_self → dispatch_auto_bind_lease_with_source → worktree_pool::lease`
(`dispatch_hook/mod.rs:92`). Tested by
`bind_self_rejects_main_branch_with_e4_5` (worktree.rs:442).

**`handle_watch_ci` (`src/mcp/handlers/ci/mod.rs:159-254`) does NOT call
`worktree_pool::lease` — and contains zero E4.5 / protected-ref validation
of any kind.** Code path through that handler:

- Line 212: `let branch = args["branch"].as_str().unwrap_or("main");` —
  `main` is the SILENT DEFAULT when the caller omits `branch`.
- Line 220-248: creates the watch file directly, no validation gate.
- Line 254-…: appends caller to `subscribers` array, atomic-writes.

### Gap shape

Any agent (with any binding state, including no binding at all if `repo` is
passed explicitly) can subscribe to `main`'s CI surface. The watch file is
created, the daemon polls main, the agent receives `ci-pass` / `ci-fail`
notifications for every push to main.

Empirical proof from this session: at 2026-05-08T15:34Z I called
`mcp__agend-terminal__ci action=watch repo=suzuke/agend-terminal branch=main`
to follow the Sprint 56 closeout. **It succeeded silently** — no error, no
audit, no warning.

Severity MEDIUM-HIGH (per lead's framing): the violation is logical not
physical (no worktree mutation, no branch checkout), but it lets agents
develop a "watching main" habit that defeats the E4.5 "agents should not
hold any concept of interest in main" principle.

### Recommended fix shape (Phase B IMPL)

Mirror the `worktree_pool::lease` gate inside `handle_watch_ci`:

```rust
// Above line 220 in src/mcp/handlers/ci/mod.rs
if branch == "main" || branch == "master" {
    return json!({
        "error": "E4.5 violation: ci action=watch rejects protected branch — use lead/operator dashboards for main CI surveillance",
        "code": "protected_branch_watch_rejected"
    });
}
```

Considerations:

1. Extract a single `is_protected_ref(branch: &str) -> bool` helper into
   `crate::worktree_pool` (or a new `crate::protected_refs` module) so all
   E4.5 sites point at one source of truth — currently the literal
   `branch == "main" || branch == "master"` is duplicated at lease site.
2. Decide whether `lead` agent (the orchestrator) needs an explicit override
   path. Recommendation: **NO** — closeout signals on main go through the
   ci_watch the operator's `general` agent already maintains; lead consumes
   them via inbox replay, not direct subscription.
3. Migrate any pre-existing main watches on disk during the same Phase B
   commit: walk `$AGEND_HOME/ci-watches/`, drop any file whose
   `branch == "main" || "master"`, log removal.

### Tests gap

`worktree.rs:442` covers `bind_self_rejects_main_branch_with_e4_5`. Phase B
IMPL needs the parallel test `ci_watch_rejects_main_branch_with_e4_5`
(asserts `handle_watch_ci` returns the new error code) plus a migration test
for the on-disk main-watch removal sweep.

---

## Item 5 — Dedup ledger lost across daemon restart

(Lead's enumeration skips Item 4 — out-of-scope worktree placement design.)

### Status quo

`RateLimitRetry` at `src/daemon/supervisor.rs:69-93` carries the per-agent
dedup state added in Sprint 56 Track G (#529):

- `fingerprint: u64` — `DefaultHasher` hash of pending-input bytes.
- `dedup_count: u32` — injects fired for this fingerprint in the current
  window.
- `last_inject_at: Instant` — drives the window check.
- `dedup_audit_emitted: bool` — latch so the cap-hit audit fires once per
  fingerprint, not per tick.

Storage is **on-stack only**: `let mut retry_tracks: HashMap<String,
RateLimitRetry> = HashMap::new();` declared inside `run_loop`
(supervisor.rs:185). No filesystem reads, no filesystem writes, no on-shutdown
serialization.

The dedup decision is made by the pure helper `dedup_decision(retry,
current_fingerprint, now)` at supervisor.rs:134-150, which returns
`Suppress / ForceFreshContent / AllowAfterWindowReset / Allow`.

### Gap shape

Daemon restart → `retry_tracks` HashMap is empty. Any in-flight rate-limit
recovery state vanishes:

- `dedup_count` resets to 0 → cap is effectively re-armed at restart, even if
  the original cap-1-per-60s window had not yet expired.
- `last_inject_at` is gone → window-check uses a fresh `Instant::now` against
  a missing prior, so `AllowAfterWindowReset` fires immediately on the first
  post-restart tick.
- `dedup_audit_emitted` resets → the same cap-hit can re-emit audit events on
  the second cycle.

The empirical 2026-05-08 PR #547 cycle (operator rebuild+restart) **did not
fire this gap** because the post-restart inbox had no replay material — the
window simply never had a triggering second-cycle injection. **The gap is
latent**: if a daemon restart lands within a 60s dedup window AND a fresh
fingerprint-matching notification arrives within that window, dedup will
under-suppress.

### Design tension

The notification-injection family has two patterns:

- **Sprint 52 reply_to_channel** — durable inbox at
  `$AGEND_HOME/inbox/<agent>/`. Restart-replay-immune by design (agent
  consumes idempotently from inbox watermark).
- **Sprint 56 Track G dedup** — ephemeral in-memory ledger. NOT
  restart-replay-immune.

The two designs are inconsistent. Operator's choice to build `RateLimitRetry`
ephemerally was undocumented (no comment in supervisor.rs explains it). Two
plausible designs forward:

A. **Persistent ledger** (`$AGEND_HOME/dedup-state/<agent>.json`) —
   atomic-write per dedup-count bump, atomic-read on supervisor startup.
   Pros: simple correctness story; mirrors inbox durability.
   Cons: write amplification (every retry tick in steady state writes a
   file); needs migration logic for existing fields.

B. **Restart-replay-immune semantic** — re-derive `last_inject_at` from
   `last_input_text` epoch in `notification_queue` (which IS persisted) plus
   the inbox's `last_consumed_at` watermark. `dedup_count` becomes a
   computed value instead of a stored counter. Pros: zero new on-disk state;
   matches Sprint 52 philosophy. Cons: requires careful epoch reasoning
   across the restart boundary; harder to test.

### Recommended fix shape (Phase B IMPL)

**Recommend Option A** (persistent ledger) for Phase B because:

1. Lower-risk implementation — atomic-write semantics are already used at
   `crate::store::atomic_write` (referenced in worktree_pool.rs:342) and the
   on-disk schema mirrors the existing struct 1:1.
2. Single-tick latency cost is negligible (HashMap insert + one atomic-write
   per actual inject — there are at most ~20 of these per agent per hour by
   design).
3. Aligns with operator's STRICT v2 philosophy: prefer durable state for any
   correctness invariant rather than implicit reconstruction.
4. Phase B IMPL can ship Option A in a single commit; a future refactor to
   Option B is a clean follow-up if ledger size becomes a concern.

Concrete shape:

- New module `src/daemon/dedup_state.rs` with `load(home, agent)`,
  `save(home, agent, retry)`, `clear(home, agent)`.
- `run_loop` at supervisor.rs:185 calls `load_all(home)` once at startup to
  hydrate `retry_tracks`.
- `process_server_rate_limit_retries` calls `dedup_state::save` after every
  inject (including suppress emit) and `clear` on recovery (Ready/Idle).
- Schema versioned (`schema_version: 1`) so future migrations are safe.
- TTL: discard entries whose `last_inject_at` is > 24h old at load time.

### Tests gap

Existing dedup tests at supervisor.rs::tests cover the in-memory state
machine (Suppress / ForceFreshContent / AllowAfterWindowReset / Allow).
Missing: **restart-replay test** — write a `retry_tracks` snapshot to disk,
spawn a fresh `run_loop`, assert the supervisor sees the persisted state and
applies dedup correctly on the first post-restart tick within a 60s window.

---

## Phase B IMPL prereq summary

For Phase B dispatch, the per-item dependencies are:

| # | Module surfaces | New helpers | Migration / data | Test harness |
|---|-----------------|-------------|------------------|---------------|
| 1 | `daemon/ci_watch.rs` | `gc_expired_watches(home)` | none | restart-with-stale-file test |
| 2 | `worktree_pool.rs` | `unsubscribe_all_ci_watches_for_agent(home, agent)` | none | bind→ad-hoc-watch→release test |
| 3 | `mcp/handlers/ci/mod.rs` + new `protected_refs.rs` | `is_protected_ref(branch)` | drop existing main-watches at startup | `ci_watch_rejects_main_branch_with_e4_5` |
| 5 | `daemon/supervisor.rs` + new `daemon/dedup_state.rs` | `load_all` / `save` / `clear` | versioned schema | restart-replay-with-pending-dedup test |

Items 1, 2, 3 are independent and can ship as a single PR (~150-250 LOC
total). Item 5 is independent and slightly larger (~150-300 LOC + tests),
recommended as a separate PR.

Suggested Wave 2 dispatch:

- **Track B (Wave 2)**: Items 1+2+3 bundled — single PR, reviewer Tier-1
  codex single primary.
- **Track C (Wave 2)**: Item 5 standalone — single PR, reviewer Tier-1
  codex single primary, second reviewer recommended given persistence
  design implications.

---

## Verdict — Phase A status

All four items audited. Empirical evidence from this dev agent's own session
on 2026-05-08 directly demonstrates Items 2 and 3 firing in production
(ad-hoc main watch added during Sprint 56 closeout, release_worktree did not
clean it). Items 1 and 5 are latent but pinned via static analysis.

Phase A deliverable complete. Phase B IMPL clear to dispatch with the
fix-shape recommendations above.

---

## Out of scope — not addressed in Phase A

- **Item 4** — worktree physical placement design (P2 Wave 4, operator
  Option A vs B pending decision).
- **Phase B IMPL** — any production code change.
- **gh `--delete-branch` ergonomic** — surfaced separately during Sprint 56
  closeout (Track I-Phase2c merge action), Wave 2 parallel candidate per
  lead's queue.

## References

- Sprint 56 Track I-Phase2c closeout: PR #547 → main `8725118`.
- Sprint 55 P0-B EC7 (`unsubscribe_ci_watches_for_release` introduction):
  `worktree_pool.rs:292-349`.
- Sprint 56 Track G (#529) dedup: `supervisor.rs:69-150`.
- Sprint 52 reply_to_channel pattern (referenced as design analogue for
  Item 5): inbox durability under `$AGEND_HOME/inbox/<agent>/`.
- E4.5 enforcement at lease site: `worktree_pool.rs:21-32`.
- E4.5 test pin: `mcp/handlers/worktree.rs:442`.
- This RCA was triggered by lead's task dispatch
  `m-20260508155705275610-5` (2026-05-08 15:57Z) which references operator's
  Sprint 57 PLAN draft P-ranking + general dispatch
  `m-20260508155605565292-4`.
