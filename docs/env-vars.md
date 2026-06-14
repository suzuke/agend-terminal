[繁體中文](env-vars.zh-TW.md)

# Environment Variables Reference

A categorized reference for every `AGEND_*` environment variable read by the
codebase, plus the honored external/standard variables and test-only fixtures.

Every entry was derived by reading the **actual read site** (`std::env::var` /
`var_os` / `has_env`) and its default-resolution logic — not inferred from the
name. `file:line` points at the primary read site; line numbers are relative to
the crate root and reflect `origin/main` at the time of writing.

## Conventions

- **Presence-based** flag: enabled by the variable merely being *set*
  (`var_os(name).is_some()` / `var(name).is_ok()`), value ignored.
- **Value-based** flag: enabled only when the value matches a specific string
  (commonly exactly `"1"`); any other value — including empty — is "off".
- **Default** describes behavior when the variable is **unset** (the `unwrap_or`
  fallback or "feature off"). Unparseable numeric values generally fall back to
  the same default.
- 🔒 **Secret** — never log, print, echo, or commit a value. No example values
  are shown for these.
- ⚠️ **Security-sensitive** — changing it weakens an enforcement boundary or
  triggers destructive/irreversible behavior.

---

## 1. Core / identity

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_HOME` | Overrides the core agend home directory (state / runtime / `.env` root). Also consumed by the git shim, mcp-bridge, agent, and claim_verifier. | `~/.agend` if it exists (else legacy `~/.agend-terminal` for back-compat). In the git shim, unset → empty → shim execs real git unmodified. | Absolute directory path. | `src/main.rs:111` (primary); also `src/bin/agend-git.rs:66`, `src/bin/agend-mcp-bridge.rs:486`, `src/agent/mod.rs:1963` | Operator-facing core config. Heavily used in tests for isolated homes. |
| `AGEND_INSTANCE_NAME` | The agent's identity name. Daemon injects it into each spawned agent's env; read back to stamp the "from" on cross-instance messages and to authorize bind / CI actions. | Unset/empty → `None` = anonymous / standalone mode; identity-gated handlers reject anonymous callers. No literal default name. | String restricted to `[A-Za-z0-9_:-]`, non-empty. | `src/identity.rs:29` (canonical read); also `src/bin/agend-git.rs:107`, `src/mcp/handlers/ci/mod.rs:776` | ⚠️ Security-sensitive (gates bind/CI authority). Listed in `SENSITIVE_ENV_KEYS` so templates cannot override it. Presence distinguishes agent-caller vs operator-shell. |

---

## 2. Channels & tokens

All bot tokens are read **indirectly**: a fleet.yaml `bot_token_env` field names
the variable, whose value is then fetched via `std::env::var(bot_token_env)`.
The variables below are the **default names** for that indirection.

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_TELEGRAM_BOT_TOKEN` | Default name of the env var holding the Telegram bot token. | Config field defaults to the name `AGEND_TELEGRAM_BOT_TOKEN`; if that var is unset at read time, falls back to legacy `AGEND_BOT_TOKEN` (with deprecation warning). | Telegram bot token string. | `src/fleet/mod.rs:227` (default name); read at `src/channel/telegram/creds.rs:23` | 🔒 Secret. Operator-facing. |
| `AGEND_DISCORD_BOT_TOKEN` | Default name of the env var holding the Discord bot token. | Token var unset → Discord channel not activated (no-credentials arm). | Discord bot token string. | `src/fleet/mod.rs:230` (default name); deref read at `src/channel/telegram/creds.rs:23` | 🔒 Secret. Operator-facing. Shares the telegram channel's token indirection. |
| `AGEND_BOT_TOKEN` | **Legacy/fallback** Telegram bot token, read only when the configured `bot_token_env` var is unset; emits a deprecation warning steering operators to `bot_token_env`. | Both unset → "bot token env not set" error; telegram verify test is skipped. | Telegram bot token string. | `src/channel/telegram/creds.rs:25`; `src/channel/telegram/bootstrap.rs:39` | 🔒 Secret. **Deprecated** — read-time fallback only. `quickstart` now writes the canonical `AGEND_TELEGRAM_BOT_TOKEN` (and migrates a legacy line out on re-run); prefer `bot_token_env` in fleet.yaml. |
| `AGEND_TELEGRAM_GROUP_ID` | Telegram supergroup id `quickstart` reads (when set) to seed the generated fleet.yaml `group_id`, so onboarding can pre-fill the channel binding from the environment. | Unset → `quickstart` leaves `group_id` unseeded (operator fills it in fleet.yaml / via topic binding). | Telegram chat/supergroup id string (e.g. `-100…`). | `src/quickstart.rs:174` | Operator-facing, read only at `quickstart` onboarding time (not on the hot path). Not a secret. |

---

## 3. Supervision & restart

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_WRAPPED` | Restart-supervisor marker set by `scripts/agend-wrapper.sh` before each daemon start; one signal that a supervisor will respawn the daemon on `exit(42)`, letting `restart_daemon` proceed. | Absent → contributes no supervised signal; if no other signal is present, `is_restart_supervised()` is false and restart fails closed. | **Presence-based** (`var_os(...).is_some()`); any value, even empty, counts. | `src/daemon/restart.rs:55` (`has_env`, lines 62–63) | ⚠️ Security-relevant: fail-closed gate on the destructive `restart_daemon` path. See also external `XPC_SERVICE_NAME` / `INVOCATION_ID` below, and the positive `AGEND_SUPERVISED` sentinel + the `AGEND_RESTART_HANDOFF` / `AGEND_SUCCESSOR_HANDOFF` rows below. |
| `AGEND_SUPERVISED` | Positive supervisor sentinel written into the generated launchd plist / systemd unit by `service install`; `is_restart_supervised()` accepts it as proof a supervisor will respawn the daemon on `exit(42)`. Replaced the ambient `XPC_SERVICE_NAME` false-positive. | Absent → contributes no supervised signal. | **Presence-based** (`has_env`); the templates write `=1`. | `src/daemon/restart.rs` (`SUPERVISED_ENV`, `is_restart_supervised`) | #1812. ⚠️ Security-relevant (same fail-closed gate as `AGEND_WRAPPED`). |
| `AGEND_RESTART_HANDOFF` | On/off switch for the #1814 self-healing successor-handoff restart path (spawn a successor, health-gate it, abort-stay-alive on failure) vs the legacy `exit(42)` + external-respawn fallback. | **Unset → ON** (self-respawn) since Stage 4. `=0` → legacy `exit(42)` path (byte-identical to pre-#1814). | **DEFAULT ON**: only the literal `"0"` opts out; unset / `"1"` / anything else ⇒ self-respawn. | `src/daemon/restart.rs` (`self_respawn_enabled`, `RESTART_HANDOFF_ENV`) | #1814 Stage 4 flipped the default from opt-in to opt-OUT (after Stage 2 aligned launchd `KeepAlive` to `SuccessfulExit=false`). **#2098**: independent of this flag, `restart_daemon` fail-closes in `agend-terminal app` (combined TUI+daemon) / any non-`run_core` owned mode — that process has no in-process `RESTART_PENDING` consumer, so an in-process self-respawn would brick the control plane. There: quit + relaunch the app, or SIGTERM + restart. Gated by the positive `RUN_CORE_ACTIVE` marker (`src/daemon/mod.rs`). |
| `AGEND_SUCCESSOR_HANDOFF` | Internal handoff token (`<old_pid>:<token>`) the predecessor sets on the successor it spawns, so the successor takes the minimal pre-lock handoff boot (bypassing the singleton "another daemon is already running" guard, deferring flock + reconciles). NOT an operator knob. | Unset → normal boot (full `prepare`). | `<u32 pid>:<non-empty token>`; malformed → ignored (normal boot). | `src/daemon/restart.rs` (`successor_handoff_marker`, `SUCCESSOR_HANDOFF_ENV`) | #1814 — internal; set only by `spawn_successor_handoff`. |

---

## 4. Auto-recovery

The Stage gates are **value-based** (must equal `"1"`); when off they run in
"shadow mode" (telemetry/logging only, no live action). A separate runtime-config
master gate (`hang_auto_recovery_enabled`) can also enable Stages 1–3.

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_AUTO_RECOVERY_STAGE1` | Stage 1 gate: write ESC byte to a hung agent's PTY. | Inactive (shadow mode) unless master gate on. | `"1"` enables; else off. | `src/daemon/per_tick/recovery_dispatcher.rs:193` | Operator flag; mutates a live PTY. |
| `AGEND_AUTO_RECOVERY_STAGE2` | Stage 2 gate: emit `Stage2Restart` event (restarts the agent). | Inactive (shadow mode) unless master gate on. | `"1"` enables; else off. | `src/daemon/per_tick/recovery_dispatcher.rs:155` | Operator flag; triggers agent restart. |
| `AGEND_AUTO_RECOVERY_STAGE2_MAX_RESTARTS` | Max Stage 2 restart attempts. | `3` (`STAGE2_MAX_RESTARTS_DEFAULT`). | `u32`. | `src/daemon/per_tick/recovery_dispatcher.rs:161` | Safety bound on restart loops. |
| `AGEND_AUTO_RECOVERY_STAGE3` | Stage 3 gate: escalate by writing `HealthState::Paused`. | Inactive (shadow mode: telegram + tracing only) unless master gate on. | `"1"` enables; else off. | `src/daemon/per_tick/recovery_dispatcher.rs:114` | Operator escalation gate. |
| `AGEND_PRODUCTIVE_GATE` | Activates the F9 "productive-silence" hang-detection path (can flag an agent Hung). Off → shadow telemetry only. | `false` (inactive). | `"1"` activates; else off. | `src/health.rs:753` | Rollout feature gate. |

---

## 5. Worktree & GC

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_WORKTREE_AUTO_CLEANUP` | Gates the runtime worktree auto-cleanup sweep (removes merged worktrees, prunes orphan branches). | **On** (`unwrap_or(true)`). | Any value except `"0"` → enabled; only `"0"` disables. | `src/worktree_cleanup.rs:17` | Opt-**out**. ⚠️ Module doc comment says "`=1` opt-in" but code is opt-out — code is authoritative. |
| `AGEND_WORKTREE_ENFORCEMENT` | In the messaging handler, whether a task target must be bound in a managed worktree before delivery. | `"warn"` (log a warning but allow delivery). | `"off"` (skip), `"enforce"` (reject `worktree_not_managed`), else (incl. `"warn"`) → warn-and-allow. | `src/api/handlers/messaging.rs:282` | ⚠️ Security-sensitive (gates messaging unbound agents). Tri-state, not a bool. |
| `AGEND_WORKTREE_GC` | Master gate for the worktree GC sweep (archives clean orphan worktrees to `.trash`, purges old trash). | **Off** (no-op). | `"1"` enables; else off. | `src/daemon/retention/worktrees.rs:391` | ⚠️ Gates worktree deletion. Strict `=="1"`. |
| `AGEND_WORKTREE_GC_TRASH_DAYS` | Retention window (days) for `.trash/worktrees/*`; older entries purged on GC sweep. | `7`. | `u64` days; `0` = purge same sweep; no positivity filter. | `src/daemon/retention/worktrees.rs:49` | Tuning knob. |
| `AGEND_WORKTREE_FORCE_RECLAIM_DAYS` | Age cap (days) for the force-reclaim backstop: a never-released lease with no agent liveness older than this is force-reclaimed. | `7` (also when `<=0`). | `i64` days, filtered `>0`. | `src/worktree_pool.rs:534` | ⚠️ Controls destructive reclaim. |

---

## 6. Git-shim & bypass

These live in the `agend-git` shim binary (`src/bin/agend-git.rs`). The three
`AGEND_GIT_BYPASS*` controls are layered emergency overrides — all ⚠️ security-sensitive.

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_GIT_BYPASS` | Layer-1 one-shot override: if set, `should_bypass()` returns true and execs real git, skipping all enforcement. | Not bypassed. | **Presence-based** (`is_ok()`); any value, even empty. | `src/bin/agend-git.rs:178` | ⚠️ Emergency override. Daemon-internal callers set `=1` to skip the shim. |
| `AGEND_GIT_BYPASS_AGENT` | Layer-2 agent-specific exemption: bypass when value equals the current `AGEND_INSTANCE_NAME`. | No agent exemption. | Agent/instance name string. | `src/bin/agend-git.rs:181` | ⚠️ Value-compared against `AGEND_INSTANCE_NAME`. |
| `AGEND_GIT_BYPASS_UNTIL` | Layer-3 time-limited exemption: bypass while now < the given epoch. | No time exemption. | **Unix epoch seconds** (`u64`, not ISO). | `src/bin/agend-git.rs:188` | ⚠️ Expired/unparseable → no bypass. |
| `AGEND_GIT_SHIM_DEPTH` | Recursion guard propagated into spawned git; hard-fails at `MAX_SHIM_DEPTH = 3`. | `0`. | Non-negative `u32`; unparseable → 0. | `src/bin/agend-git.rs:33` (read); set at `:1279`, `:1310` | Internal sentinel; not normally user-set. Exits 70 when `>= 3` (#1504). |
| `AGEND_REAL_GIT` | Escape hatch holding the path to the real git binary so the shim execs git without recursing. Daemon injects it at agent spawn. | Shim: unset → falls back to literal `"git"` then PATH-exclude resolution. Daemon: only injected when not already set. | Absolute path to a git executable; accepted only if it exists. | `src/bin/agend-git.rs:1338` (read); injected at `src/agent/mod.rs:835` | ⚠️ Correctness-sensitive: wrong/missing value risks recursive-spawn storm (#1504). |

---

## 7. Logging & diagnostics

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_LOG` | `EnvFilter` directive controlling log verbosity for CLI and the rolling daemon/app subscribers. | CLI → `agend_terminal=info`; rolling → caller-supplied default filter. | `tracing_subscriber::EnvFilter` syntax, e.g. `agend_terminal=debug`, `info,agend_terminal::daemon=trace`. | `src/logging.rs:76` (CLI), `:156` (rolling) | Operator-facing. |
| `AGEND_LOG_MAX_BYTES` | Directory-size backstop: hourly handler prunes oldest `daemon.log.*` until total footprint is under the cap. | `2 GiB` (`DEFAULT_MAX_BYTES`). | Plain bytes or `K`/`M`/`G` suffix (case-insensitive), e.g. `2G`, `500M`. | `src/daemon/per_tick/log_rotation.rs:43`; parser `src/logging.rs:198` | Daemon-only tuning. |
| `AGEND_LOG_RETAIN_DAYS` | `max_log_files` on the daily rolling appender (count of rotated daily files retained). | `3` (`DEFAULT_RETAIN_DAYS`; also when `<=0`). | Positive `usize`. | `src/logging.rs:62` | Orthogonal to the byte cap. |
| `AGEND_DAEMON_THREAD_DUMP_SECS` | Per-tick thread-dump interval (seconds); `N>=1` enables periodic dumps. | `0` / disabled. | `u64` seconds; `0` disables. | `src/sync_audit.rs` (`thread_dump_interval_secs`) | Cached once via `OnceLock` — restart to toggle. Single accessor feeds both the handler interval + the `thread_dump_enabled` gate. |
| `AGEND_DEBUG_PTY_READ` | In the PTY read loop, enables verbose debug logging of read counts/byte totals. Debug-only seam. | Off. | `"1"` enables; any other value (incl. `0`) is off. | `src/agent/mod.rs` | Internal debug flag. |
| `AGEND_LOCK_AUDIT` | Enables lock-ordering audit in **release** builds (logs tier violations instead of being a no-op). | Release build → no-op; debug/test builds always audit regardless. | **Presence-based** (`is_err()` check). | `src/sync_audit.rs:43` | Dev/diagnostic; affects release builds only. |
| `AGEND_TUI_SIZE_DEBUG` | #2057 instrument: logs the controlling TTY's kernel winsize at named app-startup milestones (to trace where the TUI's own render area shrinks). | Off (no size tracing). | Value-based: exactly `"1"` enables; else off. | `src/app/mod.rs:270` | Internal diagnostic (`app` mode only). Read once at startup into a local so there is no per-frame env-lookup cost. |

---

## 8. Watchdog & recipients

> **Deprecation (watchdog topology → `fleet.yaml`).** The five `AGEND_IDLE_WATCHDOG_*`
> / `AGEND_TASK_STALL_RECIPIENTS` / `AGEND_DECISION_TIMEOUT_RECIPIENT` vars below are
> agent / recipient **names** — fleet *topology*, not env tuning. Their home is the
> `fleet.yaml` top-level `watchdog:` block (see `docs/FEATURE-fleet.md`). The env vars
> remain as a **deprecated fallback for one window** so existing setups keep working;
> resolution precedence is **fleet.yaml `watchdog:` value > env var (deprecated, warns
> once) > built-in default**. Move them to `fleet.yaml` and drop the env vars:
>
> ```yaml
> watchdog:
>   idle_watchdog_agent: dev          # AGEND_IDLE_WATCHDOG_AGENT (single-agent mode)
>   dev_recipient: lead               # AGEND_IDLE_WATCHDOG_DEV_RECIPIENT
>   fleet_recipient: lead             # AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT
>   task_stall_recipients: [general, lead]   # AGEND_TASK_STALL_RECIPIENTS
>   decision_timeout_recipient: general      # AGEND_DECISION_TIMEOUT_RECIPIENT
> ```

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_IDLE_WATCHDOG_AGENT` | **Deprecated** → `watchdog.idle_watchdog_agent`. Single-agent mode for the dev-vantage idle watchdog (watch only this agent). | `"dev"` (load-failure fallback only). | Agent name; empty/whitespace ignored. | `src/fleet/watchdog.rs` | Fleet config wins; env is the deprecated fallback. |
| `AGEND_IDLE_WATCHDOG_DEV_RECIPIENT` | **Deprecated** → `watchdog.dev_recipient`. Recipient for dev-vantage idle alerts. | `"lead"`. | Recipient name; empty/whitespace ignored. | `src/fleet/watchdog.rs` | Fleet config wins; deprecated fallback. |
| `AGEND_IDLE_WATCHDOG_FLEET_RECIPIENT` | **Deprecated** → `watchdog.fleet_recipient`. Recipient for fleet-vantage idle alerts ("whole fleet is quiet"). | `"lead"`. | Recipient name; empty/whitespace ignored. | `src/fleet/watchdog.rs` | Fleet config wins; deprecated fallback. |
| `AGEND_TASK_STALL_RECIPIENTS` | **Deprecated** → `watchdog.task_stall_recipients`. Recipients of task-stall warnings. | `["general", "lead"]`. | Comma-separated names; entries trimmed, empties filtered. | `src/fleet/watchdog.rs` | Fleet config (list) wins; deprecated fallback. |
| `AGEND_DECISION_TIMEOUT_RECIPIENT` | **Deprecated** → `watchdog.decision_timeout_recipient`. Recipient for the decision-timeout auto-default (operator-proceed) emission. | `"general"`. | Non-empty recipient name; blank treated as unset. | `src/fleet/watchdog.rs` | Fleet config wins; deprecated fallback. |
| `AGEND_WATCHDOG_DRY_RUN` | Makes the per-tick watchdog log classified PTY errors to the event log only instead of mutating agent health state. | `false` (health mutations applied). | `"1"`/`"true"`/`"TRUE"`/`"True"` → dry-run; else off. | `src/daemon/watchdog.rs:21` | Operator safety toggle. |

---

## 9. MCP & tools

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_HOOK_STATE_POC` | Lifecycle-hook state gate (#1523 epic / #2016 promotion). When on: (a) the MCP-config writer injects hook state-reporters into the agent's per-workspace `.claude/settings` (scope-respecting; user-global `~/.claude` untouched), and (b) for a **hook-instrumented (strong) backend**, a *fresh* hook-derived `AgentState` is **promoted to authoritative** in the daemon's per-tick snapshot — winning over the screen heuristic. | Off (no reporters injected; hook state never promoted; the screen heuristic drives everything — byte-identical). | Value-based: exactly `"1"` enables; else off. | `src/mcp_config.rs:193` (inject); `src/daemon/hook_shadow.rs:115` (`promotion_enabled`), `:148` (`authoritative_state`); `src/daemon/per_tick/snapshot.rs:51` (snapshot adopts it) | Internal feature gate, default-OFF. **Promotion is phased-v1, SNAPSHOT-scoped** (#2014): it drives snapshot consumers — `dispatch_idle`, the pane-state badge, `agent_state_of`/`snapshot.json` (the #1985 surface). A stale/unknown hook window, the flag off, or a non-hook backend ⇒ heuristic fallback (unchanged). Per-tick deciders that read the RAW screen heuristic directly — supervisor, hang detection, recovery dispatcher, idle/anti-stall watchdog, `conflict_notify`, `query`/`list` API — are **not** promoted in v1 (epic phase-2, post-soak). |

---

## 10. Env-isolation & security

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_ENV_ISOLATION` | Gate for agent-backend env isolation (#1440 phased rollout). | Disabled. | `"1"` enables; else off. | `src/agent/mod.rs:179` | Default-off feature flag. When on, only allowlisted env is forwarded to backends (see [external env](#12-honored-external-env)). |
| `AGEND_ALLOWED_ROOTS` | Extra allowed root directories for `working_directory` validation (appended to home, workspace, cwd). | No extra roots; only home + workspace + cwd allowed. | OS-path-separator list (`:` Unix, `;` Windows); empty segments skipped. | `src/api/mod.rs:156` | ⚠️ Controls path-traversal allowlist for agent working dirs. |
| `AGEND_BIND_STRICT_MODE` | In dispatch_hook: when `source_repo` resolves to a stub (tier 4) and this is `"1"`, reject the stub fallback, forcing an explicit `source_repo` in fleet.yaml. | Strict mode off; stub fallback allowed. | `"1"` enables; else off. | `src/mcp/handlers/dispatch_hook/mod.rs:343` | Production safety gate. |

---

## 11. State-detection, injection & timing/tuning

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_POINTER_ONLY_INJECT` | When on, PTY inbox injection uses header-only ("pointer") format, forcing agents to call `inbox` for the body. | `false`. | `"1"` enables; else off. | `src/daemon_config.rs:27` (seeds `DaemonConfig::default`); consumed via `src/inbox/notify.rs:14` | Env read only at default-construction; runtime value lives in `DaemonConfig`. |
| `AGEND_CONTEXT_ALERT_PCT` | Context-window usage percent at which the per-tick context-alert watchdog notifies (with hysteresis + re-alert cadence). | `80.0` (`DEFAULT_ALERT_PCT`). | Float percent; unparseable → default. | `src/daemon/per_tick/context_alert.rs:36` | Tuning knob. Operator-facing. |
| `AGEND_CONTEXT_HANDOFF_PCT` | Context-window usage percent at which the context-handoff watchdog injects a `SESSION-HANDOFF.md` request to the agent. | `85.0` (`DEFAULT_HANDOFF_PCT`). | Float percent; unparseable → default. | `src/daemon/per_tick/context_handoff.rs:51` | Tuning knob. Should sit above the alert pct. |
| `AGEND_CONTEXT_HANDOFF_ESCALATE_PCT` | Higher context-window percent at which the handoff watchdog escalates to the operator. | `92.0` (`DEFAULT_ESCALATE_PCT`). | Float percent; unparseable → default. | `src/daemon/per_tick/context_handoff.rs:58` | Tuning knob. Should sit above the handoff pct. |
| `AGEND_LOW_DISK_THRESHOLD` | Free-space floor (bytes); inbox writes treat available space below this as "low disk". | `1 GiB` (`1024³`, `DEFAULT_LOW_DISK_FLOOR_BYTES`). | `u64` bytes; unparseable → default. | `src/inbox/disk.rs:13` | Tuning knob. Plain byte count (no `K`/`M`/`G` suffix, unlike `AGEND_LOG_MAX_BYTES`). |

---

## 12. Daemon lifecycle / retention / capture

| Name | Purpose | Default (unset) | Valid values / format | Source | Notes |
|------|---------|-----------------|-----------------------|--------|-------|
| `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` | Enables the destructive boot-time zombie-daemon sweep and sets the age threshold (days); candidates older than N are killed. | Telemetry-only (no kills); threshold const `DEFAULT_AGE_DAYS = 14`. | Positive integer `>=1` days; malformed → warn + treated as unset. | `src/daemon/boot_sweep.rs:36` | ⚠️ **Destructive** (SIGTERM/SIGKILL zombie daemons). Setting a valid value flips destructive mode on. |
| `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN` | Secondary gate: when `"1"` and age-days is set, downgrades the destructive sweep to log-only. | Not dry-run (destructive if age-days set). | `"1"` enables dry-run; else off. | `src/daemon/boot_sweep.rs:40` | Safety override for the sweep. |
| `AGEND_RETENTION_CUTOVER` | Kill-switch for the **pending-dispatch** retention sweep (deletes dispatch sidecars older than 14d). | **On** unless `=="0"`. | `"0"` disables; unset / anything else → enabled. | `src/daemon/retention/mod.rs:41` | Opt-**out**. #env-cleanup: decoupled — this is now pending-dispatch ONLY (decisions moved to its own flag below). `=="1"` ALSO still enables the decisions sweep as a legacy fallback (deprecated). |
| `AGEND_RETENTION_DECISIONS_CUTOVER` | Opt-in gate for the **decisions** retention sweep (archives expired decisions). | **Off**. | `"1"` enables; else off. | `src/daemon/retention/decisions.rs` (`decisions_cutover_enabled`) | Opt-**in**. #env-cleanup decouple: own flag so `pending-OFF + decisions-ON` is reachable. Legacy `AGEND_RETENTION_CUTOVER=1` still enables it (deprecation window). |
| `AGEND_FLEET_NO_AUTO_MIGRATE` | Disables automatic backfill/migration of missing instance IDs in `fleet.yaml` during load. | Auto-migration runs (backfills IDs and rewrites fleet.yaml). | `"1"` skips migration; else off. | `src/fleet/mod.rs:544` | Opt out of auto-rewrite. |
| `AGEND_CAPTURE_FIXTURES` | Activates the PTY-capture fixture sink: raw PTY bytes written to `$AGEND_HOME/captures/<agent>/`. Boot path emits a privacy warning. | `NoOpCapture` (no capture, zero overhead). | `"1"` enables; else off. | `src/capture.rs:56`; `src/bootstrap/mod.rs:224` | ⚠️ Fixture-capture tool, readable on the real boot path. Captured bytes may contain **secrets/prompts** — review before committing. See also [test-only](#14-test-only-fixtures). |

---

## 13. Pending / in-flight (not yet on `main`)

These are designed but not yet merged. Documented here for forward reference;
verify against code once their PRs land.

_None currently._ (The previously-listed `AGEND_SUPERVISED` (#1812) and
`AGEND_RESTART_HANDOFF` / `AGEND_SUCCESSOR_HANDOFF` (#1814) have merged — see
their permanent entries in [§3. Supervision & restart](#3-supervision--restart).)

---

## 14. Test-only fixtures & seams

⚠️ **Do not set these in production.** They are test-harness conventions /
fixtures, or **test-only seams** whose env read exists ONLY so a cross-process
integration test (which spawns an agend binary as a subprocess) can control
that subprocess's timing — production always uses the fixed default. They are
**not production tunables**.

| Name | Purpose | Source | Notes |
|------|---------|--------|-------|
| `AGEND_BRIDGE_TOOLS_LIST_TIMEOUT_MS` | **Test-only seam.** Shortens the `agend-mcp-bridge` `tools/list` retry-timeout budget so a cross-process test sees the daemon-unreachable error fast. Production always uses the fixed **30 s** default. | `src/bin/agend-mcp-bridge.rs:271` (read); set by `tests/attached_path_mcp_invariants.rs` | Not a production tunable (#env-cleanup reclassify). `u64` ms; malformed → default. |
| `AGEND_SPAWN_STAGGER_MS` | **Test-only seam.** Sets the multi-agent staggered-spawn delay in a spawned daemon so a cross-process test gets a deterministic startup-race window. Production always uses the fixed **500 ms** default. | `src/daemon/mod.rs:1091` (read); set by `tests/ready_marker_invariants.rs`, `tests/attached_path_mcp_invariants.rs` | Not a production tunable (#env-cleanup reclassify). `u64` ms; unparseable → default. |
| `AGEND_SELF_RESPAWN_SETTLE_SECS` | **Test-only seam.** #1814 self-respawn settle window before the final recover-as-primary recheck. Production always uses the fixed **1 s** default; this exists ONLY so a cross-process integration test can widen the window deterministically (so the successor's death lands inside the recheck). | `src/daemon/mod.rs` (`self_respawn_settle`) | Not a production tunable. `u64` secs; unparseable → 1s. Widened by `tests/self_respawn_handoff.rs`. |
| `AGEND_FORCE_SUCCESSOR_FAIL` | **Test-only seam.** #1814: makes a spawned self-respawn successor crash on launch (fails the Phase-1 gate). | `src/daemon/mod.rs` (`run_successor_handoff`) | Drives the "successor-fails → predecessor stays alive (ok:false)" integration test. |
| `AGEND_FORCE_SUCCESSOR_FAIL_AFTER_CONTROL_READY` | **Test-only seam.** #1814: successor passes Phase-1 (answers STATUS) then dies before the flock — exercises the predecessor's commit→exit liveness recheck. | `src/daemon/mod.rs` (`run_core` handoff branch) | Drives the FIX2 abort-stay-alive integration test. |
| `AGEND_FORCE_SUCCESSOR_FAIL_DURING_TEARDOWN` | **Test-only seam.** #1814: successor survives Phase-1 + the loop-break recheck, then dies during the predecessor's teardown window — exercises the final recover-as-primary gate. | `src/daemon/mod.rs` (`run_core` handoff branch) | Drives the recover-as-primary integration test (with `AGEND_SELF_RESPAWN_SETTLE_SECS`). |

---

## 15. Honored external env

Standard / third-party variables the codebase actually reads (confirmed read
sites only). When `AGEND_ENV_ISOLATION=1`, a broader **allowlist** of locale /
proxy / platform vars is forwarded to spawned backends (bulk passthrough via
`std::env::vars()`, not individual reads).

### Directly read

| Name | Purpose | Default (unset) | Source | Notes |
|------|---------|-----------------|--------|-------|
| `GITHUB_TOKEN` | GitHub API auth; top of the token-discovery chain before `gh auth token`; `Bearer` header for CI/PR polling. | Falls back to `gh` CLI, else unauthenticated (60/hr). | `src/github_token.rs:166`; `src/daemon/ci_watch/provider.rs:745` | 🔒 Secret. (`GH_TOKEN` is **not** honored — only `GITHUB_TOKEN`.) |
| `GITLAB_TOKEN` | GitLab API auth (`PRIVATE-TOKEN` header). | Falls back to `~/.config/glab-cli/config.yml`. | `src/daemon/ci_watch/provider.rs:776` | 🔒 Secret. |
| `BITBUCKET_TOKEN` | Bitbucket auth (`user:app_password`). | Falls back to `~/.config/bb/config`. | `src/daemon/ci_watch/provider.rs:1001` | 🔒 Secret. |
| `HOME` | Home dir: `~` expansion, XDG fallback base, service unit paths, CLI-config fallback for tokens, backend session dirs. | Hard error for service/`~` paths; skip elsewhere. | `src/service/macos.rs:17`; `src/connect.rs:62`; `src/agent/mod.rs:630` | Unix-centric. |
| `XDG_CONFIG_HOME` | Base dir for the systemd user unit path. | `$HOME/.config`. | `src/service/linux.rs:17` | Linux. |
| `XDG_DATA_HOME` | Resolve canonical opencode `auth.json`. | `$HOME/.local/share`. | `src/agent/mod.rs:627` | XDG semantics. |
| `PATH` | Prepend agend bin / shim dir; locate real `git`/`gh`; foreign-repo detection. | `unwrap_or_default()` / harness fallback `"/usr/bin:/bin:/usr/local/bin"`. | `src/connect.rs:120`; `src/bin/agend-git.rs:1353` | Cross-platform. |
| `SHELL` | Command launched for the `Shell` backend / terminal spawns. | `crate::default_shell()`. | `src/backend.rs:144`; `src/app/mod.rs:354` | Unix. |
| `LANG` | If unset, daemon injects `LANG=en_US.UTF-8` into the spawned agent env. | Inject default if unset; left untouched if set. | `src/agent/mod.rs:762` | Cross-platform. |
| `TZ` | IANA timezone for schedule evaluation (first source before `iana-time-zone`). | Platform TZ, then `"UTC"`. | `src/schedules.rs:33` | Cross-platform. |
| `COLORTERM` | Detect 24-bit truecolor (`truecolor`/`24bit`) for rendering. | `unwrap_or_default()` → no truecolor. | `src/vterm.rs:71` | Cross-platform. |
| `TERMINAL` | Preferred terminal emulator for the tray "open terminal" action. | Try `x-terminal-emulator`, then fallback chain. | `src/tray/terminal/linux.rs:23` | Linux. |
| `USERNAME` | Windows current-user identifier (`DOMAIN\USER`) for the scheduled-task XML. | `unwrap_or_default()` → empty. | `src/service/windows.rs:22` | Windows. |
| `USERDOMAIN` | Windows domain prefix for the user identifier. | Bare `USERNAME`. | `src/service/windows.rs:23` | Windows. |
| `XPC_SERVICE_NAME` | macOS launchd supervisor-detection signal gating `restart_daemon`. | Not detected (fail-closed). | `src/daemon/restart.rs:55` (`has_env`) | macOS. ⚠️ Known false-positive source — see #1812 and `AGEND_SUPERVISED` (Pending). |
| `INVOCATION_ID` | systemd supervisor-detection signal gating `restart_daemon`. | Not detected (fail-closed). | `src/daemon/restart.rs:55` (`has_env`) | Linux. |
| `GIT_DIR` / `GIT_COMMON_DIR` / `GIT_WORK_TREE` | Presence makes the git shim fail-closed (skip foreign-repo protection) since they retarget git independently of cwd. | Normal `.git` discovery. | `src/bin/agend-git.rs:443`–`445` (`var_os`, presence only) | Cross-platform. |

### Forwarded to backends when `AGEND_ENV_ISOLATION=1` (allowlist passthrough)

Matched against `BASE_ENV_ALLOWLIST` and injected if present (absent keys simply
not forwarded). Read in bulk via `std::env::vars()` at `src/agent/mod.rs:716`;
allowlist at `src/agent/mod.rs:124`.

- **Locale / session:** `USER`, `LOGNAME`, `LANGUAGE`, `LC_ALL`, `LC_CTYPE`, `LC_MESSAGES`, `SSH_AUTH_SOCK`, `XDG_CACHE_HOME`, `XDG_RUNTIME_DIR`, `TMPDIR`, `TMP`, `TEMP`
- **Proxy:** `http_proxy`, `https_proxy`, `all_proxy`, `no_proxy` (+ uppercase variants)
- **Windows platform:** `SYSTEMROOT`, `SystemDrive`, `windir`, `PATHEXT`, `COMSPEC`, `USERPROFILE`, `HOMEDRIVE`, `HOMEPATH`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`, `ProgramFiles`, `ProgramFiles(x86)`, `NUMBER_OF_PROCESSORS`, `PROCESSOR_ARCHITECTURE`
- **Backend credentials** (🔒 forwarded to the detected backend's child; keys at `src/backend.rs:68`): `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_APPLICATION_CREDENTIALS`, `KIRO_API_KEY`, `OPENCODE_CONFIG`, `OPENCODE_API_KEY`

### Searched but NOT read in `src/`

`GH_TOKEN`, `RUST_LOG` (consumed by `tracing-subscriber` internally, no explicit read), `RUST_BACKTRACE`, `NO_COLOR`, `COLUMNS`/`LINES` (size comes from the PTY), `TERM` (only written, never read), and `XDG_RUNTIME_DIR`/`TMPDIR`/`USER` as direct reads (allowlist passthrough only).

---

## 16. Appendix: `AGEND_*` identifiers that are NOT live env vars

A `grep -rhoE 'AGEND_[A-Z0-9_]+' src/` surfaces identifiers that are **not**
runtime environment variables, so this reference deliberately omits them from the
tables above. Listed here so the inventory is provably complete (every grep hit
is accounted for).

**Demoted to fixed consts (`#env-cleanup`, single-user-dev YAGNI).** Once
env-overridable, now hard-coded; the name survives only in an explanatory code
comment, with **no `env::var` read**:

- `AGEND_API_CALL_TIMEOUT_SECS` (`src/api/mod.rs:880`) — now fixed 30 s.
- `AGEND_API_MAX_CONNS` (`src/api/mod.rs:270`) — now a fixed const.
- `AGEND_DRAFT_ESCAPE_SECS` (`src/notification_queue.rs:103`).
- `AGEND_FRAME_LIMIT` (`src/framing.rs:14`).
- `AGEND_OSCILLATION_GUARD_WINDOW_SECS` (`src/state/mod.rs:453`).
- `AGEND_PANE_INPUT_THRESHOLD_SECS` (`src/daemon/supervisor.rs:431`).
- `AGEND_PR_STATE_REPLAY_AGE_HOURS` (`src/daemon/pr_state/mod.rs:651`).
- `AGEND_WORKTREE_FORCE_RECLAIM_BOOT_GRACE_SECS` (`src/worktree_pool.rs:631`).

**String constants / markers (never an env var):**

- `AGEND_BLOCK_START` / `AGEND_BLOCK_END` (`src/instructions.rs:104`–105) — the
  `<!-- agend:start -->` / `<!-- agend:end -->` instruction-block markers.
- `AGEND_GITIGNORE` (`src/instructions.rs:53`) — a `.gitignore` body constant.

**Comment-only (not wired):** `AGEND_RENDER_DEBUG` (`src/render/core_render.rs`) —
referenced in render diagnostics comments; no `env::var` read exists.

**Test-internal fixture:** `AGEND_TEST_ENV_UTIL_FIXTURE` (`src/env_util.rs:88`) —
a key used only by `env_util`'s own unit test.