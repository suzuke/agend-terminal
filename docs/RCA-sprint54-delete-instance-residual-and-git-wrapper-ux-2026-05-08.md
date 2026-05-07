# RCA — Sprint 54 P1-B: delete_instance name residual + git wrapper UX

**Date**: 2026-05-08
**Author**: dev (kiro-cli, fleet member)
**Scope**: RCA-only per operator structural-op embargo. No production code change. Doc-only deliverable.
**Decision ref**: `d-20260507195647299987-4`
**Task ref**: `t-20260507195725672910-4`
**Audit ledger**: `d-20260507062618667443-1` (silent-drop systematic prevention framework)

---

## Bug 1 — `delete_instance` name residual

### Symptom (operator-reported)
`list_instances` (MCP) returns the post-delete fleet **without** the deleted name, but a subsequent `create_instance` with that same name fails with `agent '{name}' already exists`. The two views are inconsistent at the moment between delete-completion and the next create-attempt.

### Code path trace

**Delete path** (`mcp` → `api`):

1. `src/mcp/handlers/instance.rs:120` `handle_delete_instance(home, args)`
   - Loads `fleet.yaml`, runs the channel-singleton guard (`config.channel.is_some() && instances.len() <= 1` → reject), then calls `full_delete_instance(home, name)`.
2. `src/mcp/handlers/instance.rs:166` `full_delete_instance(home, name)` performs:
   - `crate::api::call(method::DELETE, {name})` — daemon-side teardown.
   - `crate::fleet::remove_instance_from_yaml(home, name)` — fleet.yaml removal (logs warn-on-error, **does not abort** on failure).
   - `telegram::delete_topic(home, topic_id)` if topic resolved.
   - `cleanup_working_dir(home, name, &wd)` if `working_dir` resolved.
   - `crate::teams::remove_member_from_all(home, name)` (Sprint 54 fleet-yaml unification: now writes through `fleet::update_team_in_yaml` / `remove_team_from_yaml`).
3. Daemon-side `handle_delete` (`src/api/handlers/instance.rs:92`):
   - Early-exit branch: `agent::lock_external(ctx.externals).remove(name)` returns `Ok` if the agent was external — **`full_delete_instance` continues regardless** because `crate::api::call(...)` discards the JSON response (`let _ = …`).
   - Otherwise: `crate::daemon::lifecycle::delete_transaction(...)` (`src/daemon/lifecycle.rs:75`) which kills the process tree, awaits actual exit (Sprint 20 F2 fix), removes the registry entry (Step 4, `src/daemon/lifecycle.rs:108-112`), drops the active-channel binding, removes from configs map, removes the IPC port, and emits the event log.
   - `crate::daemon::poll_reminder::remove_agent(name)` (Hotfix H3).

**List path** (`mcp` → `api`):

4. `src/mcp/handlers/instance.rs:12` `handle_list_instances` calls `crate::api::call(method::LIST)` then merges per-instance metadata from `crate::agent_ops::merge_metadata` (which reads `home/metadata/{id}.json` or the legacy `{name}.json`).
5. `src/api/handlers/query.rs:10` `handle_list` reads **only** `agent::lock_registry(ctx.registry)` + `agent::lock_external(ctx.externals)`. Both are in-memory runtime maps.

**Create-rejection path** (per call site):

6. `src/mcp/handlers/instance.rs:542` `spawn_single_instance` builds an `existing` set from `fleet.yaml` `instance_names()` and **auto-dedups** the requested name (no error). The deduped name is then passed to `api::method::SPAWN`.
7. `src/api/handlers/instance.rs:124` `handle_spawn` rejects with `agent '{name}' already exists` when `agent::lock_registry(ctx.registry).contains_key(name)` is true.
8. `src/api/handlers/external.rs:7` `handle_register_external` rejects with `agent '{name}' already exists (managed)` when the **managed** registry contains the name, and `… already exists (external)` when the **external** registry contains it. Both checks fire — no auto-dedup at this layer.
9. `src/connect.rs:34` `agend-terminal connect` (CLI helper) calls `LIST` and rejects if the returned `agents[].name` matches.

### Hypothesis: divergent post-delete data stores

Three distinct stores hold instance-name-bearing state:

| # | Store | Cleared by | Rejection sites that read it |
|---|---|---|---|
| A | `agent::AgentRegistry` (in-memory `Mutex<HashMap<String, AgentHandle>>`) | `delete_transaction` Step 4 (after child exit) | `handle_spawn:132`, `handle_register_external:16`, `handle_list:11`, `handle_inject:15` |
| B | `agent::ExternalRegistry` (in-memory `Mutex<HashMap<String, ExternalAgentHandle>>`) | `handle_delete` early-exit OR `handle_kill` external branch | `handle_register_external:20`, `handle_list:34`, `connect.rs` (via LIST) |
| C | `fleet.yaml` `instances:` block (`crate::fleet::FleetConfig`) | `full_delete_instance:181` (`remove_instance_from_yaml`, warn-on-error) | `spawn_single_instance:555` (auto-dedup, **not** rejection) |

Auxiliary state that **carries the instance name** but does not currently surface in any rejection path:

- `home/metadata/{name}.json` (legacy) → `home/metadata/{id}.json` (Sprint 46 P2). Symlink/copy left behind by `metadata_path_resolved` in `src/agent_ops.rs:97` is not cleaned by `delete_transaction`. Read by `merge_metadata` (`src/agent_ops.rs:132`) — affects `list_instances` enrichment, **not** the rejection path. Stale metadata after delete has been observed in operator chats (separate audit).
- `home/inbox/{name}.jsonl` (or id-based via `inbox_path_resolved` `src/inbox.rs:255`) — orphaned messages survive delete; not name-rejection-relevant but could surprise re-creates that share the name.
- `home/runtime/{agent}/binding.json` (per-instance worktree binding consumed by `agend-git`) — not cleaned on delete; goes orphan-stale (defended by `agend-git.rs:109-113`).
- `crate::dispatch_tracking` store (`home/dispatch.json`-ish) — instance names appear as `to: …`. Not a rejection store.
- `home/snapshot.json` (used by `handle_status`, not `handle_list`) — gitignored, refreshed on demand. Not a rejection store.

### Hypothesis status

**CONFIRMED via code-trace**: A, B, C are three independent stores. The `list` view reads A∪B; the rejection view at `handle_spawn` reads A only; at `handle_register_external` reads A∪B; at `spawn_single_instance` reads C (for auto-dedup). Cleanup paths:

- A: cleaned by `delete_transaction` Step 4 (synchronous on `delete_transaction` return).
- B: cleaned by `handle_delete`'s early-exit branch — **only** when the agent was external. If the agent was managed AND somehow also present in B (cross-lifecycle race or operator-direct API call), B is NOT cleared by the managed path.
- C: cleaned by `full_delete_instance:181` — **best-effort**, `let _ = … remove_instance_from_yaml(...)` style: errors are warn-logged but the function still returns success. A simultaneous fleet.yaml writer (Sprint 54 fleet-yaml-teams unification's `add_team_to_yaml` etc.) can race the file lock; on failure, C still holds the name.

**Most likely production scenario** (RCA-grade hypothesis, not a confirmed repro):
- Operator runs `delete_instance(name=foo)`.
- A: cleared. B: was empty for managed agents → no-op. C: lock contention → fleet.yaml still has `foo`.
- `list_instances` reads A → empty → reports no `foo`.
- Subsequent `create_instance(name=foo)` reads C in `spawn_single_instance:555` → C still has `foo` → auto-dedups to `foo-XXXX` (no rejection!).
- **BUT** if the operator's path goes through `register_external` (not `create_instance`) — that handler reads A∪B with NO auto-dedup. If the operator-reported scenario involves `register_external`, A∪B residual would surface as `… already exists (external)` or `… already exists (managed)`.

**Alternative hypothesis** (also consistent with symptom):
- A simultaneous reconcile / replay path re-inserts the name into A after `delete_transaction` Step 4 but before the operator's next `create_instance`. Candidates:
  - `replay_missed_at_startup` (`src/daemon/mod.rs:930`) — only fires at daemon startup, unlikely mid-session.
  - `auto_start_fleet` from fleet.yaml — reads C; if C wasn't cleaned (per scenario above), the daemon could re-spawn the dead-named instance into A on the next reconcile tick.

### Proposed fix design (high-level, NOT IMPL)

Two complementary changes, both low-risk:

1. **Single audit-of-name function** — introduce `pub fn name_residual_anywhere(home, name) -> Vec<&'static str>` that returns the list of stores containing `name`. Used by:
   - `full_delete_instance` post-delete: log a warn with the residual list if any (so the operator sees in `event-log.jsonl` exactly which store leaked).
   - `handle_spawn` / `handle_register_external` rejection messages: include the store name (`already exists in: registry, fleet.yaml, ...`) so the symptom is self-diagnosing.
   - This is the audit-ledger pattern from `d-20260507062618667443-1` — surface the divergence, don't paper it over.
2. **`full_delete_instance` becomes transactional-or-loud** — change `let _ = … remove_instance_from_yaml(...)` to abort + roll-back A's removal if C clean fails. Failing loud is preferable to silent residual.

Out of scope for this fix proposal (separate Sprint):
- Cross-store ID-based identity (Sprint 46 P3 was supposed to canonicalize on `InstanceId`; the rejection sites still key on `name`).
- Metadata / inbox / binding orphan cleanup on delete (audit-ledger candidate; today they survive intentionally for post-mortem inspection).

### Tier estimate
**Tier-1 single primary** — bounded scope, additive (new fn + 1-line change in 3 callers), no new architectural surface. Risk: behaviour change in `full_delete_instance` failure mode (silent → loud) — operator must accept that delete now surfaces fleet.yaml lock contention.

### Risk profile
- **Low**: The audit fn is purely diagnostic.
- **Medium**: Making `full_delete_instance` transactional changes the failure surface — agents that previously kept running on lock-fail would now see explicit error. Operator acceptance required before IMPL.

---

## Bug 2 — `agend-git` wrapper init-only-virtual-view

### Symptom (first-person evidence from PR #506)

While implementing the Sprint 54 layer-5 broadcast visibility fix (PR #506, `t-20260507180159625181-1`), running `git status` returned `working tree clean` despite ~13 modified files in `/Users/suzuke/Documents/Hack/agend-terminal/`. `git remote -v` returned empty. `git log` showed only `dde65a6 init (agend-terminal)` — one commit, no PR history. Yet `/usr/bin/git status` (bypass) showed 13 modified files, the real `origin/main` remote, and 100+ commits of PR history.

The wrapper's view → "init-only virtual view": one commit, no remote, no modifications, hides the real repo state.

This is reproducible on every fleet-managed agent. Today (this RCA's session), my live binding is:

```json
{
  "agent": "dev",
  "branch": "sprint54-p1b-rca-doc",
  "source_repo": "/Users/suzuke/.agend-terminal/workspace/dev",
  "task_id": "t-20260507195725672910-4",
  "worktree": "/Users/suzuke/.agend-terminal/workspace/dev/.worktrees/dev",
  "version": 1
}
```

The bound `worktree` directory contains only `.agend-managed` (lease marker) and `.git` (a `gitdir:` pointer file) — no `src/`, no `Cargo.toml`. The operator's actual source repo is at `/Users/suzuke/Documents/Hack/agend-terminal/` (a wholly separate path with its own `.git/` and its own remote).

### Code path trace

`src/bin/agend-git.rs` (274 LOC):

1. **`main`** (L19-47): reads `AGEND_INSTANCE_NAME` + `AGEND_HOME`, calls `read_binding`, then `classify(subcommand, args, binding)` → dispatches `Action::{Passthrough, ChdirPass(wt), Deny(reason)}`.
2. **`read_binding`** (L85-115): loads `home/runtime/{agent}/binding.json` into `Binding { task_id, branch, worktree }`. Treats parse failure as unbound (fail-safe). Treats missing-worktree-path as unbound (orphan defense P0-1.6).
3. **`is_bound`** (L117-119): bound iff `task_id.is_some()`.
4. **`classify`** (L129-192): the silent-fallback site.
   - Read-only commands (`status`, `log`, `diff`, `show`, `blame`, `ls-files`, `ls-tree`, `rev-parse`, `fetch`, `remote`, `branch`, `tag`, `describe`, `shortlog`, `reflog`):
     - bound + worktree path resolves → `ChdirPass(wt)` → wrapper runs `git -C {wt} {subcmd} {args...}`.
     - Otherwise → `Passthrough`.
   - Mutating commands (`commit`, `push`, `pull`, `reset`, …): denied if unbound, ChdirPass'd if bound.
   - `worktree`: always denied (`fleet-managed — use agend-terminal worktree tools`).
5. **`exec_real_git`** (L196-222): on Unix uses `Command::exec()` (process replacement); when `chdir` is `Some`, prepends `-C {dir}` to the real-git arg list.

The wrapper does **not** emit a marker on `ChdirPass` — `git -C {worktree} status` returns the worktree's repo state with no indication that `cwd != where-the-data-came-from`.

### Hypothesis: silent-fallback to bound-worktree-view masks `cwd` repo

**CONFIRMED via code + live binding**:

- In a bound agent, **every** read-only git query in the wrapper's vocabulary list is silently chdir'd to the binding's `worktree`. The agent has no signal that it is seeing a different repo than the directory it `cd`'d into.
- When the binding's worktree is a daemon-managed sentinel directory (lease marker only) rather than a populated checkout — the operator's actual codebase is in a parallel path — `git status` etc. report the sentinel directory's state ("init only, no remote, no modifications"). The agent reasonably concludes "no work to commit" or "no remote to push to," makes destructive recovery moves (e.g. resetting branches that aren't where the wrapper claimed they were).
- This is precisely the pattern that bit me on PR #506: I committed to `main` (real repo's checked-out branch was `sprint54-layer5-broadcast-visibility` per `/usr/bin/git`'s view; wrapper's `git rev-parse --abbrev-ref HEAD` returned the worktree-binding's branch = `main` because the worktree's HEAD was on a different ref). Recovery required `git branch <feature> <commit-sha> && git reset --hard origin/main && git switch <feature>` after the fact.

### Proposed fix design (high-level, NOT IMPL)

Two-tier remediation, ordered by user-visibility:

1. **Make ChdirPass visible** — when wrapper chdirs, prepend (or append) a single line to stderr:
   ```
   agend-git: ran from worktree binding {wt} (cwd={cwd}, agent={agent}, task={task_id})
   ```
   Suppressible via `AGEND_GIT_QUIET=1` for non-interactive scripts.

   Trade-off: extra stderr noise on every read query, but it's the cheapest signal that prevents the entire silent-fallback class. Agents parsing git output already strip ANSI / blank lines; this is a single deterministic line.

2. **Cwd-mismatch reject (opt-in via env)** — when bound + the agent's `cwd` is outside the worktree path AND the user did not explicitly pass `-C` themselves, wrapper either denies or warns. Default: warn-only to avoid breaking existing flows; `AGEND_GIT_STRICT=1` upgrades warn → deny.

Out of scope for this fix proposal:

- Daemon-side worktree provisioning (today the lease worktree is a sentinel-only directory; populating it with a real checkout is a separate Sprint task — `repo` MCP `checkout` action exists but the daemon does not invoke it on lease-grant).
- Bidirectional sync between operator-source-repo edits and lease-worktree state (the deeper architectural mismatch — agent edits land in operator path, wrapper claims the worktree path is canonical).

### Tier estimate
**Tier-2 dual reviewer** — wrapper changes affect every fleet agent, and the cwd-mismatch reject change has surface-area in CI / scripted git invocations that may rely on the silent ChdirPass. Recommend dispatching `reviewer` (codex) for code review + a second reviewer for fleet-protocol impact (does this break §10.4 worktree-mandate workflows?).

### Risk profile
- **Low** for tier-1 (visibility line): output-only, no semantic change.
- **Medium-High** for tier-2 (cwd-mismatch reject): opt-in via env, but the env-default behaviour change has fleet-wide blast radius. Phased rollout (warn for one Sprint, then strict by default) recommended.

---

## Cross-bug observation — daemon-view-inconsistency family

Both bugs surface the same architectural shape:

> **Daemon-projected view ≠ underlying truth**, with no signal to the agent that the projection is happening.

| Aspect | Bug 1 (`delete_instance` residual) | Bug 2 (`agend-git` virtual view) |
|---|---|---|
| Source of truth | A∪B∪C (registry + externals + fleet.yaml) — three independent stores | Real git repo state at `cwd` |
| Daemon-projected view | `list_instances` reads A∪B only | `git status` reads `binding.worktree` only |
| Mutation interface | `delete_instance` MCP tool (cleans A∪B∪C best-effort) | Wrapper ChdirPass on every read query |
| Failure mode | Silent residual in C → `auto_start_fleet` resurrects deleted name | Silent worktree mismatch → agent commits to wrong branch / repo |
| Agent's signal that the projection happened | None — operator must compare two MCP calls (list + create) | None — operator must run `/usr/bin/git` to see the real state |
| Recovery cost | Re-attempt delete with bypass + manual fleet.yaml edit | Manual `git branch + reset + switch` to undo wrong-place commit |

**Why this family is a class hazard**: the agent operates on the projected view as if it were truth, and the projection's correctness depends on **multiple independent stores or paths** staying in sync. Any one of them can drift; without a pre-call signal ("I'm about to project, the real source is X"), the agent has no chance to detect drift before it acts on stale data.

**Future systematic prevention** (audit-ledger candidate — separate Sprint):

1. **Convention**: every "view-projecting" daemon-side handler emits a single-line provenance marker on each read response (e.g. `result.audit = { sources: ["registry", "fleet.yaml"], divergence_check: "passed|skipped" }`). Agents that parse the response can verify; agents that don't, can ignore.
2. **Periodic divergence audit**: a daemon-side tick (e.g. every N seconds) walks the known multi-store views and emits warn-log + telegram notify when divergence is detected. Unlike the per-call audit in (1), this catches divergence even when no agent is actively querying.
3. **Single-store invariant**: where possible, collapse multi-store views to single-store. Sprint 54's fleet-yaml-teams unification (PR #507) is exactly this pattern applied to teams data; analogous work for instance-name state (collapse A/B into a single "agents.json" derived from C, or vice versa) would close Bug 1 entirely.

---

## Summary of fix Tier estimates (NOT IMPL)

| Bug | Proposed fix | Tier | Risk |
|---|---|---|---|
| 1 | `name_residual_anywhere` audit fn + transactional-or-loud `full_delete_instance` | Tier-1 | Low/Medium (failure surface change) |
| 2 (visibility) | Wrapper stderr provenance line on ChdirPass | Tier-1 | Low (output-only) |
| 2 (cwd-mismatch reject) | Opt-in `AGEND_GIT_STRICT=1` warn → deny on cwd outside worktree | Tier-2 | Medium-High (fleet-wide blast radius, phased rollout required) |
| Cross-bug systematic | Convention: read-handler audit metadata + daemon divergence tick | Sprint-wide | Out-of-scope for this RCA |

---

## Embargo discipline confirmation

Per `t-20260507195725672910-4` operator constraint: **no production code change** in this branch / PR. `git diff --stat` against main `8dda979` shows only `docs/RCA-sprint54-delete-instance-residual-and-git-wrapper-ux-2026-05-08.md` added (this file). Verified pre-commit.
