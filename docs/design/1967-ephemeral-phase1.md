# #1967 Phase-1 — Cross-backend ephemeral worker (architecture + build order)

Status: spike VET PASS (2026-06-21). Design doc for the staged implementation.
Scope of THIS epic = **Phase 1** only (ephemeral spawn/reap + telemetry sidecar +
cost control). Phase 2 (declarative DAG engine) / Phase 3 (dynamic generation) stay
**trigger-conditioned** per the #1967 risk section — built only when Phase 1 is stable
and a real pain point appears.

## Lead VET decisions (baked-in — later PRs follow these)
- **MVP target = opencode-via-ACP** (NOT claude-SDK). One ACP client serves
  opencode + kiro (#1954) — highest leverage, proves the hardest net-new piece
  (headless protocol transport) on the highest-ROI path.
- **Telemetry = 3 layers**: L1 in-memory `FleetEvent` (real-time UX) + L2 TUI
  sub-loop + L3 `task_events::WorkflowCompleted` durable summary. **NOT** the
  `fleet_events.jsonl` file — that is a git/merge AUDIT log
  (`agend-git` shim + `mcp/handlers/ci/merge.rs:200`), not a telemetry sink.
- **`HeadlessTransport` trait** (handshake / prompt / stream / cancel); ACP impl
  first (PR3).
- **Ephemeral workers AVOID managed bookkeeping**: do NOT call registry /
  fleet.yaml / binding / worktree-pool. Tracked in their own sidecar store.
- **Cost-guard day-1**: hard max-live concurrency cap + max-wall-TTL must land in
  **PR1**, BEFORE real spawn (PR2) and BEFORE token metering (PR6).

## Backend headless matrix (#1954, verified 2026-06-11 against installed binaries)
| backend | official headless channel | Phase-1 path |
|---|---|---|
| opencode | acp + serve + attach | **ACP client (opencode+kiro) = first step** |
| kiro-cli | acp (hidden) | same ACP client |
| claude | no serve; Agent SDK (stdio/OAuth) / channels / MCP | own track (SDK > channels), later |
| codex | mcp-server (stdio) + app-server (experimental) | defer until app-server matures |
| agy | none | **unsupported** — explicit error, NO PTY fallback |
Interrupt caveat: protocol `cancel` does NOT guarantee the process stops → process
kill is the hard fallback.

---

## 1. MVP definition
Minimal end-to-end vertical slice = daemon spawns ONE non-claude headless worker →
runs a bounded task → reports via the telemetry sidecar → is reaped.
MVP backend/transport = **opencode-via-ACP** (rationale above; lead-decided).

## 2. Transport strategy (build order)
1. **ACP client (opencode + kiro) FIRST** — one client, two backends, JSON-RPC/stdio.
2. **claude SECOND — Agent SDK > channels** (SDK is purpose-built for programmatic
   headless use, OAuth-capable; channels is a chat/TUI surface). OPEN-Q: exact SDK
   invocation shape (node lib vs stdio subprocess; headless OAuth) → sub-spike.
3. **codex app-server DEFER** (experimental).
4. **agy UNSUPPORTED** — `ephemeral spawn backend=agy` → explicit error, no PTY fallback.
Abstraction: `HeadlessTransport` trait (handshake / send-prompt / stream-events /
cancel); ACP impl first.

## 3. Ephemeral lifecycle (spawn/reap MCP API)
MCP tool `ephemeral` (mirrors the action-tool scaffold — `mcp/registry.rs:146`
ALL_TOOLS + `mcp/tools.rs def_*` + `mcp/handlers/dispatch.rs:171 action_adapter!`):
- `spawn {backend, prompt, workflow_id, parent?, ttl_secs?, token_budget?}` → worker_id + pid
- `list {workflow_id?}` → running workers + age + phase + tokens
- `reap {worker_id? | workflow_id? | all_stale}` → reaped / still-running

Tracking OUTSIDE managed bookkeeping (each surface bypassed by simply NOT calling it):
SKIP registry insert (`agent/mod.rs:75`), fleet.yaml (`fleet/persist.rs add_instance_to_yaml`),
binding (`binding.rs bind_full`), worktree-pool.
Store = `ephemeral_tracking.rs` (mirrors `dispatch_tracking.rs`: JSON +
`store::mutate_versioned` atomic flock RMW). **Path: `$AGEND_HOME/ephemeral_workers.json`**
(home-root, matching every existing store — dispatch_tracking/deployments/schedules/tasks;
the spike said `state/` but no store uses a `state/` subdir, so we follow the real
convention). Fields: worker_id / workflow_id / parent? / backend / pid /
process_start_token / spawned_at / ttl_secs / token_budget? / phase / status.
RECOMMEND JSON-sidecar over in-memory → reap-on-boot cleans crash-leaked workers.

Reap = a `PerTickHandler` sweep (lives in BOTH app + run_core via
`build_default_handlers`; the live daemon is app-mode) + reap-on-boot in
`bootstrap::prepare` (near `boot_sweep_zombies`). Per worker: if `status==done` OR
`now-spawned_at >= ttl_secs` (max-wall-TTL) OR pid dead → terminate (if still alive,
token-verified) + drop entry. `process_start_token` (`process.rs:58`) guards PID
recycle. (PR1 uses single-process `terminate`; the process-GROUP `kill_process_tree`
+ protocol graceful-cancel-before-kill are PR2/PR3.)

## 4. Telemetry sidecar (3 layers) — PR4/PR5
- **L1 real-time UX**: extend `FleetEvent` (`channel/ux_event.rs:92-134`) with
  worker-lifecycle variants carrying `workflow_id/parent/phase` (additive Option
  fields — easy, no version bump; 4 existing emit sites unaffected).
- **L2 TUI subtree**: `render_fleet_view` (`render/panels_fleet.rs:83-167`) is FLAT
  today → add a sub-loop: active-workflow header + indented worker rows (workers are
  NOT panes; no layout-tree refactor). Driven by the ephemeral_tracking store.
- **L3 durable summary**: new `TaskEvent::WorkflowCompleted{workflow_id, status,
  summary, worker_count, tokens}` (`task_events.rs`, schema v2→v3, replay-exhaustive).

## 5. Cost control — PR1 (guards) + PR6 (token budget)
GAP: NO real-time token budget + NO live concurrency cap exist today. `token_cost.rs`
is OBSERVATION-ONLY (post-hoc transcript scan). Both NET-NEW.
- **Concurrency cap (PR1)**: hard `MAX_LIVE_WORKERS`; spawn admission rejects when
  live count ≥ cap, BEFORE creating the child.
- **Max-wall-TTL (PR1)**: every worker carries `ttl_secs`; the reap sweep enforces it.
- **Per-workflow token budget (PR6)**: (a) admission — sum workflow tokens vs budget;
  (b) polling kill — reap checks cumulative tokens. OPEN-Q token source: ACP usage
  messages (real-time, preferred) vs transcript scrape (post-hoc) → sub-spike.
- **Rate-limit respect (PR6)**: pre-spawn check the backend is not UsageLimit/
  QuotaExceeded (reuse `usage_limit.rs:43-79`).

## 6. Reuse map
REUSABLE AS-IS: `build_command()` (`agent/mod.rs:710`), `Backend`/`preset()`
(`backend.rs:9-30/378-663`), `process.rs` {is_pid_alive:7, process_start_token:58,
terminate:122, kill_process_tree:155}, MCP action-tool scaffold (registry/tools/
dispatch action_adapter!), sidecar-store pattern (`dispatch_tracking.rs` +
`store::mutate_versioned`), `PerTickHandler` framework + `CadenceGate`, `framing.rs`
(framed IO for the future protocol), `task_events::append`, `ux_sink_registry().emit`.
NET-NEW: headless no-PTY spawn (gate in `spawn_agent` `agent/mod.rs:1079` before
`openpty`), ACP transport, `ephemeral_tracking` store + reap, cost-control guards,
FleetEvent workflow fields + TUI subtree + task_events summary variant.

## 7. Build order (ordered PRs)
- **PR1 (this) — scaffold, NO real spawn / NO protocol / NO telemetry**: design doc +
  `ephemeral_tracking` store + `ephemeral` MCP tool (spawn FAKE child = `/bin/sleep`) +
  reap sweep (PerTickHandler) + reap-on-boot + cost-guards (max-live cap, max-wall-TTL).
- **PR2** — headless no-PTY spawn (gate at `spawn_agent`; std::process piped stdio) +
  `kill_process_tree` group-kill on reap.
- **PR3** — `HeadlessTransport` trait + ACP client (opencode) = MVP vertical slice; agy → unsupported-error.
- **PR4** — telemetry L1 + L2.
- **PR5** — telemetry L3 + add kiro (same ACP client, free).
- **PR6** — per-workflow token budget admission + polling kill + rate-limit check.
- **PR7** — claude Agent SDK transport. DEFER codex app-server.
Sub-spikes gating their PR: ACP method coverage vs delivery needs (verify per actual
binary, like #1954) → PR3; claude SDK invocation shape → PR7; token source → PR6;
portable-pty pipe-mode vs std::process → PR2 (low risk, std::process default).

## 8. Risks
- **Cost (20 workers overnight)** — headline. Day-1: hard max-live cap + max-wall-TTL
  (PR1) before token metering (PR6).
- **Interrupt** — protocol cancel ≠ process stop → `terminate`/`kill_process_tree`
  hard fallback (cancel-then-kill with deadline).
- **Backend / headless maturity** — ACP coverage may be incomplete (verify per-binary);
  codex experimental (deferred); claude SDK headless-auth unproven. Each gated by a sub-spike.
- **Leaked workers on crash** — JSON-sidecar tracking + reap-on-boot (not in-memory only).
- **Token attribution accuracy** — transcript scrape is post-hoc/cwd-keyed/claude+codex-only;
  ACP usage messages preferred but unverified — budget only as good as the token signal.
- **Scope creep** — keep to ephemeral spawn/reap; Phase 2/3 stay trigger-conditioned.

---

## PR2 (headless spawn) — landed (lead-vetted Option Y)

Replaced PR1's fake `/bin/sleep` child with a REAL headless process. Key choices:

- **Option Y (not a `build_command` refactor)**: `build_command` (`agent/mod.rs:710`)
  is ~250 lines of security-sensitive managed-hot-path code (env-isolation,
  cwd symlink-validation, git-shim PATH + `AGEND_REAL_GIT`, opencode XDG
  provisioning, fleet-env filtering) with side effects — refactoring it into a
  shared `ResolvedSpawn` was rejected as over-engineering + a managed-hot-path
  regression risk for parts PR2's stub worker doesn't use. Instead the new
  `headless::resolve_headless_command` reuses the SAME single-source helpers
  (`which` / `preset_spawn_args` / `spawn_flags` / `agent::resolve_child_env`) →
  no argv/isolation drift; `build_command` is untouched (zero PTY regression).
- **`HeadlessTransport`** (`src/headless.rs`) = lifecycle only (`spawn` + `cancel`);
  the ACP protocol methods (handshake/prompt/stream) are PR3. Captured stdin/
  stdout/stderr pipes ride on `HeadlessHandle` for PR3.
- **Admission BEFORE spawn** (r4 PR1 note): `reserve → spawn → finalize`.
  `try_reserve_slot` is a single atomic RMW (no TOCTOU); an over-cap spawn rejects
  WITHOUT creating any process. `stale_reserving` rows (crash between reserve and
  finalize, age > `RESERVE_STALE_SECS` = 60s) are reaped by the sweep; the sweep
  gates reserving rows on the stale timeout, NOT pid-liveness (pid=0 reads as dead
  and would race an in-flight finalize).
- **Reap reuses PR1** (start-token PID-recycle guard, `terminate_worker`); reap =
  single-process SIGTERM + `wait()`.

### ⚠ PR3 PREREQUISITES — deferred PTY/real-backend setup (do NOT bury)
`resolve_headless_command` applies only the transport-agnostic CORE (program,
argv, #1440 env-isolation, identity env). The following `build_command` steps are
NOT done in PR2 (the stub worker has no protocol → runs no real backend) and MUST
be added before PR3 lets a real backend run headless — each is a correctness or
SECURITY prerequisite:

1. **git-shim PATH shadowing + `AGEND_REAL_GIT` (#1504)** — without prepending
   `$AGEND_HOME/bin` to PATH, a headless backend's `git` ESCAPES the `agend-git`
   safety gate (push-denylist, worktree guards). SECURITY-critical.
2. **cwd validation + provisioning** (`api::validate_working_directory` +
   `create_dir_all` + symlink revalidation).
3. **opencode per-instance XDG isolation (#1519) + autoupdate disable (#1956/#1970)**.
4. **fleet.yaml user-env passthrough with #2106 credential filtering**.
5. terminal env (`TERM`/`COLORTERM`/`FORCE_COLOR`) + git-editor env
   (`GIT_EDITOR`/`GIT_SEQUENCE_EDITOR`/`EDITOR`/`VISUAL`) + `LANG`.

(The same list is duplicated at the top of `src/headless.rs` so it is visible
from the spawn site.)

### Windows posture
The headless spawn (`std::process`) + `process::terminate` (TerminateProcess) are
cross-platform. The real-process lifecycle tests run on ALL CI platforms incl.
windows-latest (the test spawns `/bin/sleep` on unix, `ping -n` on Windows) — so
the spawn+reap path is RUNTIME-exercised on Windows, not just compile-verified
(the xwin lesson: cross-compile-green ≠ runtime-pass). Ephemeral workers remain a
unix-first runtime feature; no backend is Windows-only.

### Honest scope
PR2 is the SPAWN MECHANISM only. A spawned worker has NO protocol driving it, so it
does no useful work until PR3 (ACP) drives the captured stdio. PR2 does NOT touch
ACP/JSON-RPC, the telemetry layers (PR4/5), token-budget enforcement (PR6), or
group-kill/graceful-cancel (later).
