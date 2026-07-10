# Deep Audit 2026-07-10 — Issue Drafts (NOT yet filed on GitHub)

> Structural multi-lens audit by `grok-soak` (5 parallel adversarial explore agents + main
> premise-check). Every issue below was **verified against actual source** (quoted file:line).
> Overlaps with resolved AUDIT2 items are excluded. Nothing here is a GitHub issue yet.
>
> **Status: drafts.** Canonical `docs/audit/` was **not** written (isolated soak workspace +
> `$AGEND_HOME/captures/audit-2026-07-10/`). Promote into the repo only after gapfix/operator triage.
>
> **Threat-model note:** same as AUDIT2 — same-UID agents, loopback cookie, self-declared
> `params["instance"]`. Severities calibrate to trusted co-worker fleets + prompt-injection depth
> and soak scale, not multi-tenant OS isolation.

Severity: **Critical** · **High** · **Medium** · **Low**.  
Counts: **5 Critical**, **12 High**, **5 Medium**, **1 Low** (23 total).

Related docs: `DEEP-AUDIT-2026-07-10-SUMMARY.md`, `DEEP-AUDIT-2026-07-10-PRIORITIZATION.md`.  
HEAD reviewed: `30489362` (`main`, includes #2707 Grok backend MVP).

> **Provenance (canonical promotion).**
> - **Source:** autonomous structural deep-audit by `grok-soak` (5 parallel adversarial
>   explore agents + premise-check), 2026-07-10, in an isolated soak workspace. Original draft:
>   `~/.agend-terminal/workspace/grok-soak/docs/audit/DEEP-AUDIT-2026-07-10-ISSUES.md`.
> - **Triage decision:** `d-20260710080227610961-3` (operator-approved). Promoted for the audit
>   trail after AUDIT3-005 was CONFIRMED as real data-loss and fixed (#2717, merged).
> - **Promoted:** 2026-07-10 by gapfix-dev3 (task `t-20260710131303015147-33689-14`). Docs-only,
>   no code changes. The per-finding **Disposition** lines below record the triage outcome;
>   findings without an explicit triage verdict are marked *open-observation* (not adjudicated).

---

## Group A — Architecture: multi-rooted control plane

Shared root cause: fleet authority is hosted by **two** process entrypoints (`daemon::run_core` and
`app::run_app`), domain logic lives under MCP handlers, and agent “state” has two authorities.

### AUDIT3-001 — Dual control plane: live fleets often never enter `run_core`
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Deferred — structural design wave (extract `RuntimeHost`); tracked as triage open-question Q1.
- **Severity:** Critical *(structural)*
- **Component:** `src/app/mod.rs`, `src/daemon/mod.rs`, `src/mcp/handlers/restart.rs`
- **Description:** Comments and history show the **live** fleet daemon commonly runs as
  `agend-terminal app` (`run_app`), which owns registry/API/per-tick/supervisor/shadow and
  **never** sets the `run_core` restart consumer. Control-plane features have repeatedly been
  “dead in app mode” until dual-wired. `restart_daemon` intentionally fails when
  `RUN_CORE_ACTIVE` is false.
- **Evidence:**
  - `src/app/mod.rs` — “LIVE fleet daemon runs `agend-terminal app` (`run_app`), NEVER `run_core`”;
    intentional allowlist of per-tick handlers; app-mode wiring for shadow/supervisor.
  - `src/daemon/mod.rs` — `RUN_CORE_ACTIVE` static; set true only on `run_core` entry.
  - `src/mcp/handlers/restart.rs` — early return when `!RUN_CORE_ACTIVE` (“not supported in app”).
- **Expected:** single composition root for fleet authority; TUI is a client **or** app/start share
  one `RuntimeHost` with zero mode-private control features.
- **Actual:** two hosts; feature skew and intentional restart gap in the interactive path.
- **Suspected root cause:** historical merge of “daemon” and “owned TUI” without extracting host.
- **Suggested fix:** extract `RuntimeHost`; make `app` attach-only **or** make restart/tick planes
  identical by construction (not allowlists).
- **Related:** #2098, #1814 self-respawn, prior silent-dead recovery/shadow in app mode.

### AUDIT3-002 — Layer inversion: daemon production paths call into `mcp::handlers`
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/daemon/**` → `src/mcp/handlers/**`, empty `src/lib.rs` boundary
- **Description:** CI auto-arm, autonomic restart, retention/dispatch helpers invoke MCP handler
  functions as domain services. MCP is not a thin adapter; layers cycle
  (`daemon → mcp → agent_ops → api → mcp`).
- **Evidence:** e.g. `src/daemon/pr_state/auto_arm.rs` → `mcp::handlers::ci::handle_watch_ci`;
  `src/daemon/per_tick/respawn_watchdog.rs` → instance_state restart helpers; `lib.rs` is test
  re-exports only (no crate layering).
- **Expected:** domain services under a non-MCP module; MCP/API are pure façades.
- **Actual:** domain co-located under MCP package; daemon cannot evolve without MCP.
- **Suggested fix:** extract `services::{ci,dispatch,lifecycle}` used by both MCP and daemon.

### AUDIT3-003 — Dual agent-state authority (raw vs operated) without single consumer contract
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/daemon/shadow/mod.rs`, recovery_dispatcher, snapshot/list paths
- **Description:** Screen heuristic `core.state.current` and Shadow `operated_state` intentionally
  disagree for some consumers (KEEP-RAW for health/recovery). Same-tick dual truth is documented
  but easy to mis-wire for new features.
- **Evidence:** `shadow/mod.rs` operated_state docs (not for SRL/health); recovery_dispatcher
  KEEP-RAW (#2465); snapshot/list use operated.
- **Expected:** explicit matrix: which subsystem may read which authority (and tests).
- **Actual:** open coordination risk (#2466 class); new code may pick the wrong plane.
- **Suggested fix:** typed accessors (`AgentStateView::Raw` / `Operated`) + compile-time or
  lint-level consumer lists; keep dual authority but make misuse hard.

---

## Group B — Messaging / coordination protocol gaps

Shared root cause: inbox three-state machine + soft protocol prompts ≠ hard daemon invariants.

### AUDIT3-004 — Implicit next-drain “ack” settles query/task without proof of handling
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Resolved — folded into #2720 (merged): a plain reply now settles the current turn's inbox rows.
- **Severity:** Critical
- **Component:** `src/inbox/storage.rs` drain
- **Description:** On re-drain, any row still in `delivering` is marked `read_at` (processed)
  because “agent polled again.” Agents that re-poll habitually silently clear blockers including
  `kind=query` / open `kind=task`. Reclaim never re-delivers.
- **Evidence:** `src/inbox/storage.rs:643-652` — `#2299 (A) implicit ack` marks prior delivering
  processed on re-drain.
- **Expected:** explicit `inbox action=ack` (or content settle via report correlation) required for
  obligation kinds; re-poll alone must not clear query/task.
- **Actual:** re-poll = handled.
- **Suspected root cause:** anti-double-deliver heuristic over-applied to obligation rows.
- **Suggested fix:** implicit-ack only for auto_ack / FYI kinds; query/task stay delivering until
  explicit ack or terminal report settle.
- **Related:** #2299, fleet protocol §4.

### AUDIT3-005 — Drain/rewrite paths drop unparseable lines (docs claim preserve)
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Confirmed → Fixed — #2717 (merged). Real durable data-loss; this finding validated the audit.
- **Severity:** Critical
- **Component:** `src/inbox/storage.rs` (drain, ack, reclaim, sweep, …)
- **Description:** Module docs say rewriters preserve every raw line verbatim; parse failures use
  `continue` and omit the line from rewrite. Half-line from crash mid-enqueue is deleted when
  `changed` is true.
- **Evidence:** doc comment ~`storage.rs:276-280` vs parse loop `from_str` → `Err(_) => continue`
  at drain (~615-617) and sibling rewrite sites.
- **Expected:** quarantine unparseable lines to `*.bad` or preserve raw.
- **Actual:** silent durable loss.
- **Suggested fix:** on parse error, append raw line to quarantine file and/or rewrite verbatim.

### AUDIT3-006 — Drain write-back failure still returns batch and marks consumed
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/inbox/storage.rs` drain tail
- **Description:** If durable rewrite fails, MCP still returns the batch; `mark_consumed` /
  reply_ledger arm still run. Disk may remain unread while dedup suppresses re-inject.
- **Evidence:** write-back `Err` only `warn`s; post-loop still iterates `newly_delivered` for
  side effects (`storage.rs` ~739-758 class).
- **Expected:** fail the drain RPC if state transition not durable; do not mark_consumed.
- **Actual:** success-shaped delivery with inconsistent disk.
- **Suggested fix:** on write error, return error empty batch; skip mark_consumed / ledger arm.

### AUDIT3-007 — Codex ack-absorption correlation ignores `task_id`
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/api/handlers/messaging.rs`, `src/inbox/storage.rs`
- **Description:** Task dispatches store id in `task_id` (often empty `correlation_id`).
  Absorption / drained-blocker checks often match only `correlation_id`, while other paths use
  `correlation_id.or(task_id)`.
- **Evidence:** messaging skip_inject / `has_drained_blocker_for_correlation` vs
  `correlation_id.or(task_id)` elsewhere in messaging.
- **Expected:** one correlation key everywhere.
- **Actual:** same-team Codex can miss PTY wake for reports that should unblock a task.
- **Suggested fix:** helper `fn corr_key(msg) -> Option<&str>` used by absorption, settle, idle.

### AUDIT3-008 — Busy gate only sees Claimed/InProgress; auto-dispatch leaves Open
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/mcp/handlers/comms_gates/dispatch.rs`, task auto-create on delegate
- **Description:** Busy filter requires Claimed|InProgress. Auto-create on `send kind=task` assigns
  but leaves **Open** → concurrent tasks all pass busy until claim.
- **Evidence:** `comms_gates/dispatch.rs` status filter; tasks Created path without auto-claim.
- **Expected:** assignee + (Open|Claimed|InProgress|Blocked) counts as busy, or auto-claim on
  dispatch.
- **Actual:** pile-up window without `force`.
- **Suggested fix:** include Open-with-assignee in busy; or claim on successful delegate.

### AUDIT3-009 — `second_reviewer` dual class only applied when `branch` is set
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** Medium
- **Component:** `src/mcp/handlers/comms.rs` + dispatch_hook
- **Description:** Gate requires `second_reviewer_reason`, but `review_class=dual` is only passed
  into lease/CI when `branch` is present. Branch-less dual-review is a free-text flag only.
- **Evidence:** `comms.rs` branch block around dual/lease; reason validation always on.
- **Expected:** dual-review either enforced on all paths or rejected without branch.
- **Actual:** silent no-op for CI dual class.
- **Suggested fix:** error if `second_reviewer` without branch, or arm dual tracking without branch.

### AUDIT3-010 — Inter-agent query/task “must reply” is prompt-only (no hard ledger)
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** Medium
- **Component:** `src/inbox/storage.rs` obligation_reason, `reply_ledger` arm condition
- **Description:** Channel user turns arm reply_ledger; peer query/task use soft obligation +
  reclaim/poll-reminder only. Protocol “query requires reply” is not a daemon hard gate.
- **Evidence:** reply_ledger arm only when `channel.is_some()` on drain; inter-agent path lacks
  equivalent escalation.
- **Expected:** optional hard must-reply for fleet query (or document as soft forever).
- **Actual:** soft only → silent stalls until human/poll.
- **Suggested fix:** either arm inter-agent reply ledger for query, or demote protocol language.

---

## Group C — Reliability / concurrency / persistence

### AUDIT3-011 — Append-only JSONL (inbox / tasks / event_log) not crash-atomic mid-line
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/event_log.rs`, `src/task_events.rs`, `src/inbox/**`
- **Description:** Flock serializes writers; crash mid-`writeln` tears a line. Multi-line batches
  fsync after whole batch. Recovery often boot-only; runtime readers skip bad lines.
- **Evidence:** append+sync patterns; comments admitting half-write; `recover_half_writes` boot
  paths.
- **Expected:** atomic record commit (length-prefix / single-fsync record / continuous repair).
- **Actual:** best-effort durability under SIGKILL/OOM.
- **Suggested fix:** record framing + periodic online repair; all-or-nothing multi-event batches.

### AUDIT3-012 — `kill_process_tree` holds child mutex across 500ms sleep
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/daemon/lifecycle.rs`, `src/process.rs`
- **Description:** Level-1 child lock held across SIGTERM → sleep 500ms → SIGKILL serializes
  teardown and any path needing `child`.
- **Evidence:** lifecycle kill under lock; `process.rs` sleep 500 between signals.
- **Expected:** drop lock before sleep; or use async wait without holding agent child mutex.
- **Actual:** soft stall under concurrent delete/restart/MCP.
- **Suggested fix:** copy pid under lock, release, then kill_process_tree.

### AUDIT3-013 — Fire-and-forget supervisor/tickers not joined on shutdown
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/daemon/ticker.rs` Drop empty, supervisor spawn, shutdown_sequence
- **Description:** Shutdown drains agents but does not join supervisor / task_sweep / shadow /
  instance_monitor threads that may still hold flocks or touch registry-adjacent state.
- **Evidence:** `DaemonTicker::drop` intentionally empty; supervisor `thread::spawn` without
  JoinHandle store; shutdown focuses on agents.
- **Expected:** cooperative shutdown + join with timeout.
- **Actual:** teardown races (documented Sprint 25+ residual).
- **Suggested fix:** `ShutdownHandles` registry; join with deadline.

### AUDIT3-014 — `snapshot.json` busy gate fail-open + up to one tick stale
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** Medium
- **Component:** `src/snapshot.rs` → reclaim / inject / dispatch-idle
- **Description:** Missing/corrupt snapshot → `agent_is_busy = false`. Stale Active can also
  mis-gate. Affects reclaim TTL and inject deferral.
- **Evidence:** `agent_is_busy` unwrap_or(false); lock-free file read design.
- **Expected:** fail-closed for reclaim of delivering obligations when snapshot unknown; or read
  live published_state atomics.
- **Actual:** fail-open not-busy under crash/before first tick.
- **Suggested fix:** prefer atomic published_state; fail-closed for reclaim only.

### AUDIT3-015 — (also Group D) see efficiency Critical for inbox O(n) drain

---

## Group D — Efficiency / scalability

### AUDIT3-015 — Inbox drain is full-file parse + full-file rewrite + fsync
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Deferred — efficiency design wave (inbox O(history) drain); measure live inbox sizes first (open-question Q4).
- **Severity:** Critical *(performance / soak)*
- **Component:** `src/inbox/storage.rs` drain
- **Description:** Every drain deserializes all rows, clones delivered msgs, and when state changes
  rewrites **entire** JSONL with durable fsync. Cost tracks history, not batch size. Multiplies by
  agent poll rate and poll-reminder scans.
- **Evidence:** drain read_to_string + line loop + full rewrite (~585-736); poll_reminder per-idle
  full scan; correlation helpers full parse.
- **Expected:** index / compact head cursor / separate active vs archive files.
- **Actual:** O(history) hot path — first bottleneck under multi-team soak.
- **Suggested fix:** active window file + archive; or embedded KV for inbox state.

### AUDIT3-016 — `list_instances` N× full pending-dispatch directory scan
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/api/handlers/query.rs`, `src/daemon/dispatch_idle/mod.rs`
- **Description:** Per instance, `pending_for_instance` re-`read_dir`s and parses all pending
  sidecars → O(A×P). Plus per-agent metadata merge disk reads.
- **Evidence:** query list loop calling `pending_for_instance`; `list_pending` full dir each time.
- **Expected:** one scan → map by instance per request (or in-memory index updated on write).
- **Actual:** classic N+1 on a heavily polled API.
- **Suggested fix:** `list_pending_index(home) -> HashMap<name, Vec<_>>` once per LIST.

### AUDIT3-017 — Task board cache hit deep-clones entire board; miss folds all archives
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** Medium
- **Component:** `src/task_events.rs`, idle_watchdog multi-callers
- **Description:** Cache hit returns `state.clone()`; miss replays archive/*.jsonl + hot (archives
  unbounded). Multiple tick callers amplify cost.
- **Evidence:** COMPACTION_KEEP 10k; clone on hit; archive dir on miss; idle_watchdog multi replay.
- **Expected:** `Arc<TaskBoardState>` cheap clone; archive GC policy.
- **Actual:** CPU/IO grows with fleet age.
- **Suggested fix:** Arc cache + archive retention/pruning.

### AUDIT3-018 — (security numbering continued below; efficiency Low notes in prioritization)

---

## Group E — Security / authority / trust boundaries

### AUDIT3-018 — `SAME_UID_OPERATOR_ISOLATION` still Unresolved (operator token on agent-readable disk)
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Tracked — duplicate of #2342 (SAME_UID_OPERATOR_ISOLATION; Conversational Phase-2 prerequisite).
- **Severity:** Critical *(security residual; blocks Conversational Phase 2)*
- **Component:** `src/auth_cookie.rs`, `src/api/operator_gate.rs`
- **Description:** `api.operator` is mode 0600 under run_dir — cross-user only. Same-uid agent can
  read it and authenticate as `Principal::Operator`. Code marks status **Unresolved** and couples
  responder-inbound tests to it.
- **Evidence:** `auth_cookie.rs:53-94`, `SAME_UID_OPERATOR_ISOLATION = Unresolved` at :95;
  operator_gate comments.
- **Expected:** principal bound to OS peer-cred or non-agent-readable secret.
- **Actual:** any shell agent → operator API.
- **Suggested fix:** UDS + `SO_PEERCRED`, or out-of-band operator token never on agent-readable disk.
- **Related:** tracking task cited in auth_cookie comments; #2342 Phase 2 prereq.

### AUDIT3-019 — Shared agent cookie + self-declared `instance` name
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `agend-mcp-bridge`, `mcp_proxy`, `identity.rs`
- **Description:** All agents share `api.cookie`. MCP identity is bridge env / params, not peer
  identity. Cookie holder can impersonate any instance for MCP tools.
- **Evidence:** bridge injects instance name; mcp_proxy trusts params; operator_gate notes
  name claim.
- **Expected:** per-agent capability cookies bound to instance.
- **Actual:** fleet-wide MCP principal with spoofable name.
- **Suggested fix:** issue per-instance cookies at spawn; reject instance≠peer binding.

### AUDIT3-020 — `AGEND_ENV_ISOLATION` defaults OFF → agents inherit daemon env secrets
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** `src/agent/mod.rs` `env_isolation_enabled`
- **Description:** Default spawn does not `env_clear()`. Children inherit operator tokens / cloud
  keys present in daemon environment. Deny-list applies mainly under isolation/passthrough paths.
- **Evidence:** `env_isolation_enabled()` true only if env `== "1"` (`agent/mod.rs:236-239`).
- **Expected:** isolation default ON for production fleets (phased rollout complete).
- **Actual:** default OFF (“does not change default spawn behavior”).
- **Suggested fix:** default ON; document break-glass `AGEND_ENV_ISOLATION=0`.

### AUDIT3-021 — `force=true` is any-agent ACL/busy override (audit theater reason)
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** High
- **Component:** tasks handler force paths, `comms_gates/dispatch.rs`
- **Description:** Non-empty `force_reason` bypasses ownership ACL and busy gates for any caller
  allowed to call the tool. No role gate / operator confirm.
- **Evidence:** force checks require only boolean + non-empty reason strings.
- **Expected:** force restricted to orchestrator/operator or dual-control.
- **Actual:** compromised implementer can force-complete or force-dispatch fleet-wide.
- **Suggested fix:** role_kind allowlist for force; or operator_mode Away-style confirm.

### AUDIT3-022 — Safety/git shim bypass env vars incomplete in deny-list / agent-settable
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** Medium
- **Component:** `agend-git`, `sensitive_env.rs`, create_instance env persist
- **Description:** Inherited `AGEND_GIT_BYPASS` stripped at spawn (#708), but agent shell can
  export bypasses; siblings (`AGEND_SAFETY_BYPASS`, `AGEND_GIT_ALLOW_CANONICAL_MUTATE`, real-git
  path overrides) not all denied on create_instance env. Kill shim documents absolute-path bypass.
- **Evidence:** agend-git bypass env docs; sensitive_env list gaps; kill_guard absolute path notes.
- **Expected:** deny-list + document “shim is footgun guard not sandbox.”
- **Actual:** adversarial agent guts shims easily.
- **Suggested fix:** extend deny-list; consider drop_capabilities if OS allows (long-term).

---

## Group F — Docs / inventory drift

### AUDIT3-023 — Architecture docs still claim wrong MCP tool counts / host model
- **Disposition (2026-07-10 triage · d-20260710080227610961-3):** Open-observation — verified against source, not yet triaged/adjudicated.
- **Severity:** Low
- **Component:** `ARCHITECTURE-REVIEW.md`, `docs/ARCHITECTURE-QUICK-START.md`, registry tests
- **Description:** Post-status review says 37 tools; registry is 29 with regression tests.
  Quick-start module names and “no auto-recovery” narrative lag code. Live host = app often
  under-documented at root.
- **Evidence:** `mcp/registry.rs` `ALL_TOOLS: [ToolEntry; 29]`; test comment on 37→29 history;
  ARCHITECTURE-REVIEW §0 tool count.
- **Expected:** docs match registry + dual-host reality.
- **Actual:** newcomer docs mislead.
- **Suggested fix:** refresh root ARCHITECTURE-REVIEW post-status; fix quick-start; point to
  `docs/architecture.md` as canonical.

---

## Appendix — Related solid areas (do not re-raise as defects)

| Topic | Why solid enough |
|-------|------------------|
| Lock tier L1→L2 + router barred | sync_audit + supervisor snapshot pattern |
| Unique-tmp atomic_write | store.rs intentional design |
| Per-tick catch_unwind | #1002 class |
| Channel allowlist fail-closed | telegram/discord inbound |
| File size ratchet | src_file_size_invariant |
| AUDIT2 SSRF/token gate | resolved lineage |
| Spawn strip AGEND_GIT_BYPASS | #708 |

---

## Appendix — Open questions for gapfix triage

1. Is **RuntimeHost** (001) a Q3 epic or opportunistic when next touching app/daemon?
2. Should **004** break agents that rely on re-poll-as-ack (protocol migration)?
3. Is **018** still hard-gating Conversational Phase 2 only, or also required before any new
   untrusted backend (Grok soak included)?
4. Performance **015/016**: measure on live `$AGEND_HOME/inbox` sizes before choosing KV vs compact
   JSONL split.
5. Any overlap with in-flight #2524 workflow-gap / #2707 work that should absorb these IDs?
