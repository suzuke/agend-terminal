# Changelog

All notable changes to this project are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); project follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [0.8.0] — 2026-06-12

### Changed

- **MSRV: declared `rust-version` corrected to 1.88 (#1994)** — the previous declaration (1.87) was false: the locked dependency set required up to rustc 1.95 (`sysinfo 0.39.3`), so `cargo install agend-terminal --locked` broke for anyone on 1.87–1.94 who trusted it. `sysinfo` is pinned to `0.38` (MSRV 1.88, no code changes — caught by the #1987 release gate on its first run, which now enforces the floor). Builders need rustc ≥ 1.88.

### Fixed

- **Rework lease conflict: reviewer-binding leak + detached-HEAD reuse (#2010 item 2)** — after a reviewer rejected a PR, re-dispatching the rework to the same branch hit a lease conflict that needed a manual `release_worktree`. Two causes: (2a) a `REJECTED`/`UNVERIFIED` verdict never enqueued an auto-release intent (the gate matched `VERIFIED` only), and even when it did the open-PR invariant held the reviewer's binding to PR-terminal — the auto-release gate is now widened to all terminal verdicts and bypasses the open-PR check ONLY for the verdict-sender's own binding once its review task is terminal and the worktree is clean (an implementer's binding is structurally untouched); (2b) reusing an agent's existing worktree whose HEAD was detached (`git branch --show-current` returns `Some("")`) always returned a lease conflict — it now reattaches the requested branch and reuses the worktree when clean, while a dirty detached worktree still conflicts (protecting in-flight review WIP). RCA credit: @cheerc.

- **Reply-ledger routes no-reply to the agent, not the operator's telegram (#2042)** — the Phase-1 audit sent a maintainer-phrased WARN (`msg Some("m-...")`) to the operator's own channel whenever an agent left a user message un-replied. Phase 2 routes the obligation to the actionable party as an escalation ladder, each rung at most once: the owing agent is nudged (with the message id + a reply-tool instruction; a failed-send gets retry wording), its lead is notified on the second miss, and the operator hears about it only as a last resort — in human phrasing. The WARN stays in the logs. Duplicate deliveries of the same logical message (operator double-send, channel redelivery replay) now group into one obligation: replying to any copy settles them all, and a redelivery of an answered message opens no new obligation — closing the false no-reply WARNs the live case hit.

- **fleet.yaml `model` now applies on runtime respawns, not just daemon boot (#2038)** — `restart_instance` / `replace_instance` / `start_instance` (and deploy/team spawns) respawned without `--model`, so a written `model:` silently ran the backend's default until the next full daemon restart. Same config-honesty class as the #900 env fix: the SPAWN handler now re-resolves `model` (and, for the arg-less replace flow, `args`) from fleet.yaml, with caller-passed `--model` still winning. Crash-respawn already preserved the boot-resolved argv and is unaffected.

- **`from_ref` resolves the correct remote in multi-remote / fork checkouts (#2010 item 1)** — `repo checkout`'s `from_ref` hard-coded `origin` for the fetch and the remote-tracking ref check, so a fork-tracking ref like `fork/main` fetched and verified against the wrong repository (latent for origin-only setups). The remote is now resolved by longest-prefix match against the actual `git remote` list — branch names containing `/` are handled correctly — falling back to origin when nothing matches. RCA credit: @cheerc.

- **Injected dispatches swallowed by an open dialog now self-heal (#2044)** — an operator-opened picker (e.g. `/model`) could swallow an injected dispatch: the keystrokes went to the dialog, the prompt never submitted, and the wake was silently lost. A new inject-delivery watchdog detects the miss dialog-agnostically (a landed inject fires a `UserPromptSubmit` hook; a swallowed one fires none) and re-delivers exactly once after 30 s, then warns and gives up — never a retry storm. Self-gated to hook-emitting backends (claude today), so a backend without that signal is never falsely re-injected.

- **MCP tool ergonomics (#2037)** — `task list` accepts `status` / `assignee` / `tag` as filter aliases (matching the documented `filter_*` args); `schedule list` trims each row's `run_history` to the newest 3 (with a `runs_total` count; `full_history=true` to opt back in); `task` accepts `task_id` wherever `id` is canonical and `decision post` accepts `text` for `content`, with error messages that teach the alias; and the never-implemented `queue=true` option was removed from `send`'s busy menu.

- **Merged-PR dispatch tasks auto-close; `create_instance` keeps an explicit name in a team (#2037)** — a dispatch task linked to a merged branch now auto-closes from any active status (was `Verified`-only), ending the daily "PR merged but the task is still open" zombies; a structured branch link closes active work while a loose title/description token match stays `Verified`-only so it can never close live work. And `create_instance` given both `name` and `team` now spawns under that exact name instead of silently renaming it to `<team>-N`.

- **Recovery telegram notice gated to acted-upon blockages (#2033)** — the recovery subsystem paged the operator's channel on recovery passes that took no action; the notice now fires only when a blockage was actually acted upon (noise reduction, #2008 class).

- **Pane content no longer floats above a blank band (#2046)** — when a pane's VTerm/PTY grid was shorter than its on-screen content area, the backend's footer / status line rendered several rows above the pane bottom with blank rows beneath it. Render now resizes the pane VTerm and underlying PTY to the actual content rect as the last-mile step, keeping `vterm rows == pane content rows` regardless of which resize path ran.

- **Inbox drain under contention is retryable, not falsely empty (#2028)** — a drain that hit lock contention returned an empty batch indistinguishable from "no messages", so a single-shot caller treated the inbox as drained and dropped the wake. Contention now surfaces as a distinct `Unavailable` signal that callers retry.

- **False AwaitingOperator after daemon restart (#2020)** — two restart shapes forced a healthy agent into `AwaitingOperator`: an idle respawned opencode pane resuming a session renders no `Ask anything` placeholder (the Idle pattern never matched → `Starting` lingered → the startup-stall fallback fired, 3× live), and a busy respawned agent injected work immediately never renders the clean ready-prompt (same fallback during a silence window). Fixes: the opencode profile gains a lowest-priority Idle pattern on the persistent statusline hint (`ctrl+p commands`; working/error patterns still win first-match — verified against the full opencode replay-fixture suite), and the startup-stall arm is vetoed when the agent has rendered backend productive markers since this spawn (a real login prompt never renders tool chrome, so the fallback's actual job is preserved; echoed injected text does not count).

- **Fresh Telegram setup resolves at startup (#2005)** — the quickstart fleet.yaml template pinned the legacy `bot_token_env: AGEND_BOT_TOKEN` while `.env` was written under the canonical `AGEND_TELEGRAM_BOT_TOKEN`, and the credentials fallback retried the same legacy name — so a fresh install's Telegram channel failed to resolve at daemon startup (old installs were masked by leftover legacy `.env` keys). The template now pins the canonical name and the fallback is symmetric (configured name → the other of canonical/legacy), so all four fleet/.env drift combinations resolve; legacy use still warns.

- **Dispatch-idle false alarms on healthy long work (#2022, #2032)** — the "dispatch has gone quiet" watchdog no longer fires on agents doing slow-but-progressing work. The deadline auto-extension is capped with a single "long-running — confirm expected" escalation instead of nagging every couple of minutes; the default idle window is raised 10 → 30 min to match real task length; and the escalation is tiered — the dispatcher is notified first, and the agent itself is only interrupted if the dispatch is still unresolved a second window later.

- **CI-watch false-storm suite (#2001, #2013)** — CI-watch now takes the verdict from the latest attempt per workflow (a rerun no longer leaves a stale failure latched), an unwatch writes a tombstone so the PR-state aggregator cannot immediately re-arm the same watch, and the handoff track is invalidated head-aware so a force-push stops re-nudging a dead commit.

- **Task board survives a corrupt event-log line (#1992)** — a single malformed line in `task-events.jsonl` no longer aborts the whole board replay; the bad line is skipped and quarantined, while a forward-incompatible (newer-schema) line still fails closed so an old daemon never acts on a board it cannot fully read.

- **Corrupt internal store is backed up, not silently overwritten (#2017)** — when a versioned store file fails to parse, the daemon moves it aside to a `.corrupt` backup and surfaces the event once per boot instead of clobbering it.

- **Git shim no longer fakes success in non-fleet repos (#2030)** — `git branch <name>` (and the other ref-naming `branch`/`tag` forms) run by a bound agent inside a repo the fleet does not manage now pass through to real git, instead of being silently redirected into the agent's worktree — which previously returned exit 0 while creating nothing, or a spurious `already exists` from the wrong repo.

- **Deferred notifications no longer strand under a headless daemon (#1978)** — a daemon-side per-tick flush delivers queued operator notifications even when no TUI is attached. (credit: @yujunchao)

- **Background services inherit `PATH` (#1984)** — `agend-terminal service install` now propagates `PATH` into the launchd/systemd service environment, so a service-started daemon can find `git`, `gh`, and the backend CLIs. (credit: @cheerc)

- **Rate-limit retry nudge requires an on-screen signal (#1999)** — the after-abort ServerRateLimit retry nudge now fires only when the rate-limit banner is actually on screen, closing a false re-nudge after the agent had already recovered. (credit: @cheerc)

### Added

- **Context-full safety net (#2007, Plan A)** — the daemon watches each agent's context usage (statusline pattern; claude-only today — other backends have no passive signal and are never injected) and at 85% injects ONE `[AGEND-AUTO kind=context-handoff]` nudge telling the agent to write `SESSION-HANDOFF.md` + annotate its task. Noise-budgeted: one injection per episode (hysteresis re-arm on compact/restart, never timer-repeated), one optional operator escalation at 92% if no handoff file appeared, silent auto-resolve, idle agents marked in the event log instead of injected. Thresholds: `AGEND_CONTEXT_HANDOFF_PCT` / `AGEND_CONTEXT_HANDOFF_ESCALATE_PCT`. Restart remains human/lead-driven (Plan B deferred).

- **`quickstart --unattended` (alias `--yes`)** — non-interactive setup for CI / scripted installs: never reads stdin (missing input = clear error + non-zero exit, not a hang), never waits on the network. Backend = first detected on PATH; Telegram optionally from `AGEND_TELEGRAM_BOT_TOKEN` / `AGEND_TELEGRAM_GROUP_ID` env (token stored unverified; the daemon validates at startup), otherwise skipped. Idempotent: an existing fleet.yaml is never overwritten; an existing `.env` token is only replaced by an explicit env var.

- **fleet.yaml `schema_version` + compatibility policy (#1989)** — `fleet.yaml` accepts an optional `schema_version:` field (omitted = `1`; existing files unchanged, the daemon never injects it). A file declaring a version newer than the daemon supports loads with a warning instead of being silently misread without trace. `docs/COMPATIBILITY.md` declares the on-disk interface tiers — (a) stable public (fleet.yaml, service templates, instruction blocks, MCP config), (b) internal persisted state, (c) regenerable/ephemeral — and the additive-only change rule for (a)/(b).

- **Release pipeline hardening** — `release.yml` gains a pre-release `gate` job (version==tag, changelog section present, MSRV 1.88 `cargo check`, `cargo-semver-checks` soft-fail report vs the previous tag) that all artifact jobs depend on, and a `publish` job that auto-publishes to crates.io after the GitHub Release succeeds (`--dry-run` first; skips gracefully when the `CRATES_IO_TOKEN` secret is unset; never runs for `-rc.N` pre-release tags). Release procedure is documented in `docs/RELEASING.md`.

- **Schema versioning for internal stores (#2000)** — `runtime-config.json`, `decisions.json`, and per-agent `binding.json` gain an optional `schema_version` (omitted = `1`; existing files unchanged), so a file declaring a version newer than the daemon supports loads with a warning instead of being silently misread — extending the #1989 compatibility policy to the (b)-tier internal persisted state.

- **CONTRIBUTING Review Process + PR compatibility self-check (#2024)** — `CONTRIBUTING.md` documents the search-first / RCA-first review flow, the VERIFIED-with-Evidence bar, the "comments and prose are claims, not evidence" rule, dual review for sensitive areas, and stale-PR carry-forward (authorship preserved); the PR template adds a self-check against the `docs/COMPATIBILITY.md` on-disk-format tiers.

- **Incident RUNBOOK for external operators (#2023)** — `docs/RUNBOOK.md` adds symptom-driven recovery recipes (daemon health, stuck dispatches, lease conflicts, channel/notification issues) so an operator without codebase context can diagnose and recover from a known failure shape.

### Removed
- **Gemini CLI backend retired** ([#1580](https://github.com/suzuke/agend-terminal/issues/1580), completes [#8](https://github.com/suzuke/agend-terminal/issues/8)). `gemini-cli` sunsets 2026-06-18 (free/Pro/Ultra); its official successor Antigravity CLI (`agy`) has been a supported backend since [#1547](https://github.com/suzuke/agend-terminal/issues/1547). The `Backend::Gemini` variant, its preset/detection patterns, and the 8 gemini state-replay fixtures are removed. **Operator note:** a `gemini` / `gemini-cli` backend named in `fleet.yaml` no longer resolves to a managed backend — it now spawns as a generic `Raw` backend. Switch such entries to `agy`. Removing the last legacy backend also let the legacy detection spine (`compile_for`, `config_for_legacy`, `legacy_initial_state`) be deleted — every backend now routes through its co-located `BackendProfile` (#8 complete).

### Changed

- **Operator-mode authority file is now fail-closed (#1576)** — on startup the daemon trusts `operator-mode.json` only if it exists and carries a valid HMAC signature; a missing, unsigned, or tampered file locks the #1339 authority gate down to restrictive **Away**. This blocks a prompt-injected agent from blind-writing `{"mode":"active"}` to disable the gate. The lockdown is **channel-agnostic** — it is enforced at the authority gate, not in any one adapter — so it suppresses operator interaction across **every** operator channel (Telegram today; Discord when built with `--features discord`; future Slack/Matrix adapters likewise): operator-authority commands arriving from a channel are not honored as operator, and agent→operator notifications are denied/queued, until a mode is activated. (Read-only and inter-agent fleet coordination are never gated.) **Migration: any install upgrading from a pre-#1576 build (before 2026-06-02) has no signed file and will start in Away — run `agend-terminal mode active` once after upgrading**, regardless of which channel(s) you use. The signed file then persists across restarts. Note: `mode get` can still report the in-memory last-known-good mode while the on-disk file is absent, so verify the file actually exists on disk, not just the reported mode.

- **Retention sweeps decoupled; `AGEND_CTRLC_SENTINEL` removed (#1812 env-cleanup)** — the decisions retention sweep now reads its own opt-in flag **`AGEND_RETENTION_DECISIONS_CUTOVER=1`**, separate from the pending-dispatch kill-switch `AGEND_RETENTION_CUTOVER` (which a co-consumer read with the opposite polarity, so "pending-off + decisions-on" was unreachable). **Migration:** the legacy `AGEND_RETENTION_CUTOVER=1` still enables the decisions sweep for now (deprecation window) — prefer the new flag. Separately, the internal Windows-debug aid `AGEND_CTRLC_SENTINEL` (wrote a sentinel file on Ctrl+C) was removed: no operator use, no automated consumer. `AGEND_POINTER_ONLY_INJECT` was reviewed and **kept** (a live inbox-injection feature flag).

- **Restart-supervisor detection: positive `AGEND_SUPERVISED` sentinel, `XPC_SERVICE_NAME` dropped (#1812)** — `is_restart_supervised()` (the #851 fail-closed guard for `restart_daemon`) no longer trusts `XPC_SERVICE_NAME`. macOS exports that variable into *every* process in a GUI login session (including a bare `agend-terminal start` from Terminal.app), so on macOS the guard returned true unconditionally and `restart_daemon` could `exit(42)` with nothing to respawn the daemon. The check now keys on an explicit `AGEND_SUPERVISED=1` sentinel that `agend-terminal service install` writes into the launchd plist (`EnvironmentVariables`) and systemd unit (`Environment=`). `AGEND_WRAPPED` and systemd's `INVOCATION_ID` remain accepted. **macOS/Linux migration: re-run `agend-terminal service install` after upgrading, then restart the daemon once** — an existing service config installed on an earlier build predates the sentinel, so until it is regenerated `restart_daemon` fails-closed with an actionable error (the safe direction — it refuses rather than bricking). Windows Task Scheduler cannot carry the sentinel (its task XML has no env element) and stays fail-closed on bare-start as before.

## [0.7.0] — 2026-05-28

200+ commits since `0.6.1` over Sprint 55–69 (May 7 → May 28, 2026). Three themes dominate:
**(1) Task board reliability** — ghost-owner class root-caused and prevented (`teams::delete` cascade, boot orphan sweep, `force` flag) + new sweepers + operator-visible health snapshot; **(2) Bridge ↔ daemon idempotent retry** — eliminates the double-execution class for side-effect MCP calls under transient transport failures (UUID request_id + DedupCache + Condvar block-wait); **(3) MCP handler refactor #694 complete** — all 30+ tool dispatch arms migrated from inline `match` to a dispatch table, paving the way for tool registry hot-reload (#776). Plus Hung detection shadow-mode foundations (F9 Stage 1–3), rate-limit recovery auto-prompt, and ~50 smaller bug fixes / hardening PRs.

### Added

- **`notify_system` helper (#1335)** — `crate::inbox::notify_system()` encapsulates the common daemon notification pattern (`InboxMessage::new_system` + builder chain + `enqueue_with_idle_hint`). Seven daemon modules migrated: `anti_stall`, `decision_timeout`, `dispatch_idle`, `fixup_nudge`, `helper_staleness_watchdog`, `idle_watchdog`, `waiting_on_stale`. Reduces boilerplate from ~8 lines to 1 per notification site.
- **Event bus (#1336)** — Global event bus behind feature flag `AGEND_EVENT_BUS=1`. `event_bus::emit_lazy(kind, || payload)` defers serialization when disabled; `event_bus::is_enabled()` allows callers to skip payload construction entirely. Zero-cost disabled path (no allocation, no serialization). In-memory broadcast channel with configurable capacity.
- **`with_pr_state` flock helper (#1342)** — `pr_state::with_pr_state()` and `with_pr_state_or_create()` serialize all read-modify-write operations on `pr-state/*.json` files via `fs4` file locks. Eliminates the lost-update race where gh-poll save overwrites scanner's `ready_emitted_for_sha` flag, causing duplicate `[pr-merged]` notifications. All 6 production `save()` call sites migrated.
- **Auto-release worktree on pr-merged (#1344)** — Scanner's `MergeState::Merged` branch now calls `auto_release_for_merged_branch()` before emitting `[pr-merged]`. Prevents `gh pr merge --delete-branch` failure when a dev's worktree still holds the local branch. Manual `release_worktree` step removed from the action checklist.
- **`/setup-telegram` skill + `skill add` URL#subdir support (#1351, PR #1354)** — per-channel install skill for guided Telegram setup. `skill add <url>#<subdir>` clones a skill repo and installs a subdirectory as a skill.
- **CI code coverage (cargo-llvm-cov + Codecov, #686)** — `.github/workflows/ci.yml` adds a `coverage` job that runs `cargo llvm-cov --workspace --tests --features tray` on Ubuntu and uploads an lcov report to Codecov. `fail_ci_if_error: false` keeps PRs mergeable when codecov.io is unreachable. Maintainer-only infrastructure.
- **Bridge ↔ daemon idempotent retry (#842, PR #843)** — bridge generates UUID v4 `request_id` per JSON envelope; retry on transport failure reuses the same id. New `src/api/request_dedup.rs` `DedupCache` (TTL 10min, 64KB/entry, 64MB ceiling, Condvar block-wait aligned to per-method timeout 5/30/60s, waiter_cap=8, RAII panic guard) caches completed responses + blocks in-flight duplicates. Caller-side retries are now safe by construction for any side-effect MCP call. Backward-compatible: missing `request_id` skips dedup (legacy clients).
- **`task action=health` (#830, PR #838)** — operator self-serve board hygiene snapshot. Returns totals + by_status + ghost_owners (strict + soft) + stale_claims + age distribution (over_30d / over_90d / median) + recommendations array. Single-call alternative to scanning the full task list when checking "is the board healthy?". Cross-references `scan_orphan_candidates` (#829) for accuracy.
- **`task action=sweep` (#806, PR #810)** — operator-triggered stale-task cleanup. 4 categories (`shipped` / `superseded` / `team_disbanded` / `validation_leftovers`) with dry-run by default + `confirm_ids` + `audit_reason` required for apply. Dual-channel audit (per-task event + event_log.jsonl `task_sweep_apply`). System identity `system:task_sweep`.
- **`repo action=cleanup_merged_branches` (#817, PR #820)** — operator-triggered local branch cleanup. 4 categories (`clean_merged` / `squash_merged` / `stale_idle` / `active_unknown`) with `apply=false` default, `confirm_ids` subset, `audit_reason` required. `min_age_days` configurable (default 90 for stale_idle). Local-only delete v1 (remote out of scope).
- **`force_release_worktree` GC mode (#826, PR #837)** — when invoked for a disbanded agent, additionally scans canonical `.git/worktrees/` for orphan git-level metadata and runs `git worktree remove --force` to clean. Closes the gap where binding cleared but git-level worktree persisted. Layered as new `src/mcp/handlers/force_release/gc.rs` module (force_release.rs split for §file-size invariant compliance).
- **Daemon boot orphan-owner sweep (#829, PR #835)** — on `start()`, daemon scans all tasks whose assignee is not in the current fleet registry. Strict mode auto-orphans (assignee=null, status preserved) + writes event_log entry. Soft mode logs only. Pubs `scan_orphan_candidates` for reuse by `task action=health` (#830).
- **`teams::delete` cascade (#828, PR #834)** — disband path now iterates team members + calls `full_delete_instance` per member, which cascades to `orphan_tasks_for_owner` (existed but never invoked on disband). Single missing wire-up that produced the ghost-owner class — fix prevents recurrence systemically. (Today's session manually cleaned 13 historical ghost-owners via the #808 force flag before this fix landed.)
- **`task force=true` flag (#808, PR #809)** — `task action=update` / `done` ACL bypass for cleanup of historical ghost-owned tasks. Requires non-empty `force_reason` (logged to event_log + per-task event). Companion to `teams::delete` cascade — `force` cleans existing pile, cascade prevents new.
- **`cleanup_init_commits` trailer-aware body gate (#833, PR #839)** — `KNOWN_TRAILER_KEYS` whitelist (`Agend-Agent` / `Agend-Task` / `Agend-Branch` / `Agend-Issued-At`) stripped before commit-body emptiness check. Production heartbeats (which carry daemon trailers via the §10.5 prepare-commit-msg hook) now correctly classified as empty + eligible for cleanup. Restores the helper from no-op state observed in pre-#833 fixup PRs.
- **`cleanup_init_commits` heartbeat synonym + body gate (#822, PR #825)** — whitelist now matches `init` + `initial`; commit must ALSO have empty body to be dropped (forward-compatible for future `wip` / `tmp` synonyms once observed).
- **`cleanup_init_commits` 5 lifecycle hooks (#789, PR #795)** — helper auto-fires on `bind_self` / `release_worktree` / `force_release_worktree` / `dispatch_auto_bind_lease` / `repo action=checkout` so callers don't need to invoke explicitly. New explicit `repo action=cleanup_init_commits` MCP for manual triggers.
- **Daemon rate-limit recovery auto-prompt (#841, PR #844)** — sibling to existing `process_server_rate_limit_retries` (which fast-retries 3×30s = 50s then exhausts). Detects `{ServerRateLimit, RateLimit, ApiError}` state after `observe_after_secs` (30s default) of idle, waits `recovery_after_secs` (60s default), then injects single-shot recovery prompt via `compose_aware_send` raw PTY (avoids `[AGEND-MSG]` header pollution + `last_input_text` clobber). `fleet.yaml` per-instance opt-out via `rate_limit_recovery: { enabled, observe_after_secs, recovery_after_secs, cooldown_secs, prompt }`. Fail-closed on config parse error.
- **`ci_watch` CONFLICTING PR detection (#813, PR #816)** — daemon now queries PR `mergeable` state on watch start; if `CONFLICTING` / `DIRTY`, emits `[ci-conflict-detected]` alert to subscribers immediately instead of polling indefinitely (GH Actions silently doesn't fire CI on conflicting PRs). Re-checks every 5min during poll (anti-spam transition logic).
- **Dispatch test-name validation (#812, PR #815)** — extends §4.3 hallucinated-fn check to reviewer-dispatch text. Daemon validates `cargo test ... <test_name>` invocations in send body against PR HEAD tree; rejects dispatch if test name doesn't exist. Eliminates the "copy-paste from previous dispatch" churn observed during the fixup batch.
- **`cleanup_init_commits` auto-recovery + threshold warn (#814, PR #818)** — helper auto-removes stale `.git/rebase-merge/` dir on retry (previous failure no longer poisons next attempt) + warns on >30 inits (complexity ceiling observation). Captures `git rebase --abort` failure for visibility.
- **Notification dedup race (#836, PR #840)** — `msg_id`-based suppression of post-consume retry re-inject. Previous behavior fired the same `[AGEND-MSG]` notification up to 3 times when the recipient hit an API rate limit mid-`inbox` call (1st send + retry-on-error + spurious 3rd). New `notification_dedup` cache (10min TTL) marks delivered notifications + filters retries.
- **Test isolation invariant (#821, PR #824)** — `tests/common/git_isolated::git()` helper enforces `current_dir(temp_dir)` + `GIT_DIR=temp_dir/.git` on every subprocess `git` call. Repo-wide grep-based invariant fails CI if any test file uses naked `Command::new("git")` without isolation. Prevents the "fixture polluted host worktree" class (observed during PR #820 prep).
- **`repo action=cleanup_init_commits` MCP tool (#789, PR #795)** — explicit operator-triggered variant of the per-lifecycle helper; useful for manual recovery when an agent's worktree has accumulated stale heartbeats outside the normal dispatch lifecycle.
- **MCP dispatch table refactor — #694 closeout** — 8-PR Block 1 (per-tick handlers extracted into `PerTickHandler` trait + Vec aggregation, #757–#760, #764) + 5-cut Block 2 (30+ tool arms migrated from inline `match` to dispatch table with action sub-routing, #765 #767 #768 #771 #777). #694 closed in PR #777. Refactor enables future tool registry hot-reload (#776) without touching dispatch sites.
- **Hung detection F9 shadow-mode foundations (#685 sub-tasks 4-7)** — productive-output gate (#766), fixture corpus + measurement harness (#769), per-backend productive markers + cache routing refactor (#770), Stage 1 auto-recovery dispatcher (#774), Stage 2 auto-restart dispatcher (#775), Stage 3 escalate + pause (#776). Shadow-mode only — no production enforcement yet. Architecture audits at #750 / #752.
- **Timezone display config (#790, PR #797)** — operator-configurable display tz via `fleet.yaml` (default Asia/Taipei). All TUI status/log surfaces use the configured tz. Resolves operator's earlier UTC vs UTC+8 mismatch confusion.
- **§4.5 cross-team ACK absorption exception (PR #638)** — protocol clarification — cross-team messages bypass the ACK absorption skip optimization. Companion fix #612 (PR #630).
- **§3.10 empirical reproduction test requirement (PR #747)** — protocol amendment: fix PRs MUST include a failing test that reproduces the bug + passes after the fix. RED→GREEN cadence enforced.
- **§3.16 Phase 1 discussion discipline, §3.17 static-review limits, §3.18 reviewer audit conflict resolution, §5 post-dispatch verification + pane-claim ≠ delivery + post-PR-merge close-loop, §6 inbox vs PTY delivery contract, §7.1 CI tool identity + cache hygiene, §7.2 cross-platform test idioms, §10.6 lead pre-dispatch release, §10.7 daemon empty-heartbeat commits, §11.1 state persistence across daemon refresh, §13.5 bug-blocks-its-own-fix BYPASS exception** (PR #805) — 12 new protocol sections distilled from session retrospectives. Plus `release_worktree` parameter form clarification.
- **§12.6 multi-PR wave sequential merge enforcement (#652, PR #663)** — protocol gate for wave-style refactors.
- **Telegram channel supervisor with auto-restart (#695, PR #734)** — long-running Telegram poll loop now resurrected on transport failure / process exit.
- **3-agent clean-shutdown smoke test (#703, PR #733)** — verifies daemon stops all agents gracefully on `SIGTERM`; pins the no-zombie invariant.
- **Bridge invariant tests (#714, PR #736)** — pins the daemon-error surfacing contract from #531 removal.
- **Top-5 MCP tools smoke tests (#699, PR #735)** — bare-minimum CRUD coverage for the most-called MCP tools.
- **`task_sweep` compliance scanner (#664, PR #676)** — Sprint 64 Wave 1 closeout. Scans tasks against config policy.
- **L4b worktree marker check (#664, PR #678)** — high-risk MCP ops require `.agend-managed` marker present.
- **Schema-enforced dispatch templates Phase 1 (#649, PR #654)** — typed shape for send/delegate envelopes.
- **CI pass auto-route to `next_after_ci` (#650, PR #657)** — `ci action=watch` accepts target agent for handoff on green.
- **`waiting_on` stale detection (#651, PR #660)** — 15min threshold alert when an agent's `waiting_on` field hasn't been cleared.
- **Safe daemon restart via `exit(42)` + wrapper script (PR #648)** — operator can `agend-terminal stop --restart` (or equivalent) for graceful restart cycle.
- **Cargo audit CI job + Dependabot config (#696, PR #718)** — security advisory tracking. Audit job permissions hardened (#831, see Fixed).
- **`docs/SKILLS.md` user guide (PR #667)** — Skills System docs.
- **Discord channel adapter docs (PR #647)** — `architecture.md` Source Layout table updated to reflect Discord adapter shipped earlier.

### Changed

- **`request_id` propagation in comms.rs (#1341)** — All three `api::call` sites in `comms.rs` now include a UUIDv4 `request_id` in the JSON envelope, enabling the daemon's `request_dedup::DedupCache` to deduplicate retries on the MCP→daemon path.
- **`dispatch_idle` flock (#1340, PR #1347)** — `mark_resolved` and `scan_and_emit` in `dispatch_idle` now use flock serialization to prevent race conditions between concurrent resolution and scan operations.
- **`agy` backend (#987)** — Google Antigravity CLI as a sixth first-class backend alongside claude / kiro-cli / codex / opencode / gemini. Motivated by Gemini CLI sunset 2026-06-18; existing `gemini` backend retained for paid Code Assist Standard/Enterprise license holders.
- **`doctor topics` taxonomy reduced from 4 classes to 2 (#994)** — `drift_fleet` and `stale_registry` classes removed. Only `live` and `orphan` remain.
- **`agy` backend display name + workspace-trust auto-dismiss + `fleet_mcp_supported: false` (#995)** — TUI shows `antigravity-cli`; workspace-trust prompt auto-dismissed; MCP config write is a no-op until upstream adds project-local `mcpServers` loading.
- **`agend-terminal list --json` envelope (#938)** — JSON output now wraps the agent array in a discriminated envelope with `mode` field.
- **`AGEND_LOG` precedence (#927 PR-A)** — env-set value now reliably wins over the in-code default.
- **`telegram_init` fire-and-forget (#945 Phase 1)** — Cold-boot wall time drops from ~6.6 s to ~0.5 s by backgrounding Telegram init.
- **CI-watch correlation_id format (#946)** — every `system:ci` inbox notification now carries `correlation_id = "{repo}@{branch}"`.
- **`dispatch_idle` watchdog fallback correlation_id (#947)** — synthesized fallback uses canonical `disp-<unix_micros>-<seq>` format.
- **`ci_watch` file identity hardening (#942 / #943)** — watch filenames now use sha256 over canonicalized repo slug. Legacy files migrated at boot.
- **`ci_watch` survives bind/release handoff (#931)** — `release_worktree`'s cleanup no longer destroys the watch file.
- **`agend-terminal admin cleanup-zombies` (#927 PR-B)** — kill long-running zombie daemons holding stale run directories.
- **Boot sweep env vars (#933)** — `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` + `AGEND_DAEMON_BOOT_SWEEP_DRY_RUN` for stale run-dir GC.
- **Thread dump env var (#941)** — `AGEND_DAEMON_THREAD_DUMP_SECS` for periodic in-process state dumps.
- **`.ready` boot-completion sentinel (#922)** — single-signal policy for daemon init completion.
- **`bootstrap-step` instrumentation (#945 Phase 0)** — per-step timing breakdown in daemon log.
- **State detection red SGR anchor (#919 Phase A)** — HIGH_FP patterns require red color escape within 200 bytes.
- **Telegram inbound length-based delivery split (#1352, PR #1353)** — short messages (<200 chars) PTY only, long messages inbox+hint. Eliminates dual-delivery duplication.
- **Replay cache mtime→generation counter (#1355, PR #1357)** — fixes false cache hits from mtime collisions by using a monotonic generation counter + mtime quadruple.
- **Default fixup batch self-merge (operator-authorized SOP)** — `impl team` / `fixup team` with three-tier composition (lead + dev + reviewer) can squash-merge after CI 5 platforms green + reviewer VERIFIED + verdict mirrored to PR comment (§3.12). No operator merge button required. Established 2026-05-13 for `agend-terminal` repo; scope is per-repo (other repos confirm separately).
- **`task action=list` default filter (#806)** — pre-#806 returned the full board (all statuses, 500KB+ in production). Now defaults to actionable statuses (`open` / `claimed` / `in_progress` / `blocked`); pass `include_history=true` to surface `done` / `cancelled`. Adds `limit` (newest-first cap) and `filtered_default: bool` response field for transparency.
- **`task action=create` response shape (#807, PR #811)** — returns full task object instead of just `{id, status: "created"}` so caller sees the same `status` field semantics across create/list/get.
- **`task.dispatched_at` → `started_at` (#807)** — field rename to reflect actual semantic (set when daemon dispatches metadata to agent, NOT when caller's `send` returned). The pre-#807 name implied caller-side dispatch ordering, which mismatched the lifecycle observed by `task action=list`.
- **`release_worktree` response shape (#807)** — `ReleaseOutcome` now serializes flat (`released` / `binding_removed` / `branch_deleted` / `worktree_removed` / `error: Option<String>`) instead of wrapped in an `<error>` envelope. Functionally equivalent but no longer looks like a failure at a glance.
- **`active:` task board indicator filters ghost agents (#827, PR #832)** — TUI bottom-status `active:` line now `filter(|name| registry.contains(name))` against the live agent list before rendering. Disbanded agents no longer flicker in the indicator even when they still own claimed tasks.
- **`docs/FLEET-DEV-PROTOCOL-v1.md` → `docs/FLEET-DEV-PROTOCOL.md`** — drop the `-v1` suffix. The document is internally versioned in its header (`v1.2`) and no v2 is in flight; the path-level `-v1` was a 2025-era misnomer suggesting a parallel v2. Compile-time `include_str!` in `src/protocol.rs`, `Cargo.toml` `[package].include` whitelist, `tests/cargo_include_invariant.rs` test mock-pattern filter, README link, `docs/ARCHITECTURE-QUICK-START.md`, and `docs/LINT-DISCIPLINE.md` updated. Operators with hand-edited overrides at the old path must rename manually (no auto-migration; mechanism rarely used).
- **MCP tools slimmed 32 → 29 (PR #640)** — three redundant tools removed during MCP refactor consolidation.
- **`replace_instance` backend from `fleet.yaml` (#662, PR #669)** — respawn now reads backend label from yaml instead of legacy in-memory cache. Companion: explicit SPAWN after DELETE step (PR #662).
- **PTY inject 2-tier format + atomic header write (#658, PR #665)** — `[AGEND-MSG]` headers and bodies now atomic per envelope; partial-write races eliminated.
- **`MCP-TOOLS.md` rewrite (#637)** — all 32 tools (now 29) catalogued with current names, actions, and schemas.
- **`source_repo` path traversal validation (#689, PR #710)** — rejects `..` segments in caller-supplied source_repo args.
- **`working_directory` canonicalize + allowed-roots (#707, PR #732)** — rejects paths outside the home / explicitly-allowed list.
- **Codex `ServerRateLimit` classifier extended (#668, PR #670)** — generic 5xx + server-side issue now classified as ServerRateLimit (triggers existing fast-retry path).
- **Audit job `permissions: checks: write + issues: write` (PR #831)** — `rustsec/audit-check@v2` POSTs check-runs to the GitHub API; the repo's `default_workflow_permissions: read` was denying the POST on every push-to-main with `Resource not accessible by integration`. Surgical permission grant scoped to the audit job. Other jobs remain read-only.
- **CI watch `[ci-pass]` dedup at action_target (#762, PR #803)** — eliminates duplicate notifications when CI completes for an action_target on success.
- **CI watch conclusion-change dedup (#786, PR #794)** — Sites 1 + 2 now share a dedup key so the same conclusion isn't surfaced twice on rerun.
- **CI watch drops stale notifications (#745, PR #746)** — when a newer commit lands on the branch, the queue clears the older watch's notification slot. Stale CI surface lifted to observable `ci-stale` inbox kind (PR #754).
- **`from:` prefix stripped from PTY header (#761, PR #798)** — `[AGEND-MSG] from=NAME` header on PTY inject no longer carries the `from:` adapter prefix that channel-aware sources used to leak.
- **Cursor-position lookup for mouse forwarding (#783, PR #801)** — mouse coordinates now resolved against actual cursor cell instead of stale cached offset.
- **Deploy preserves backend label separately from command path (#787, PR #802)** — `deployment` action no longer conflates the two on respawn.
- **Registry / team-metadata desync better-error + `Team.stale_members` field (#785, PR #793)** — `team action=list` now exposes which fleet.yaml members are missing from the runtime registry, surfacing the "binary refresh wipes registry but not team yaml" class. Daemon binding `bind_full` / `handle_watch_ci` errors hardened to no longer silently swallow partial failures.
- **Gemini submit_key `\n\r` → `\r` (#607, PR #627)** — fixes Gemini-specific stuck-on-newline injection symptom.
- **Skip PTY inject for Codex on update/report messages (#603, PR #629)** — Codex prefers stdin-only delivery for non-task messages.
- **Deployment spawn passes model + args from template (#605, PR #626)** — was previously ignored.

### Fixed

- **Filter `.lock` files from pr_state scanner (#1349, PR #1350)** — pr_state scanner was attempting to parse `.json.lock` sidecar files as JSON, producing WARN log spam every 10s tick.
- **TUI mouse selection scroll freeze (#1356, PR #1358)** — selection coordinates drifted when new output arrived during active selection. Fix snapshots `max_scroll()` at MouseDown and compensates for grid growth so the viewport stays pinned to the same content.
- **WIDE_CHAR_SPACER ratatui buffer cell leak (#819, PR #823)** — Site 1 in `src/vterm.rs::Widget::render` leaked stale chars across frames when the alacritty grid transitioned from `[WIDE_CHAR][SPACER]` to `[NarrowChar][SPACER]`. Fix relocated to the WIDE_CHAR write site (writes blank to (x+1, y) alongside the narrow char). Sites 2-5 (text/ANSI builder paths with fresh allocation) confirmed correct + explicitly regression-proofed. Surfaced as "TUI prompt-line scattered chars that disappear on selection".
- **Bridge ↔ daemon double-execution under transient transport failure (PR #804 RCA → PR #843)** — `agend-mcp-bridge::is_retriable_io` classified `io::ErrorKind::TimedOut` as retriable. Daemon-side slow handlers (telegram channel inject > 30s) tripped the bridge's `read_timeout` → retry → double execution. Fix is the L1 idempotent retry architecture (see Added). Original PR #804 (cheerc) RCA confirmed correct; closed with credit.
- **`teams::delete` did not cleanup member tasks (#828)** — single missing wire-up to `full_delete_instance` caused all ghost-owner accumulation. See Added.
- **`force_release_worktree` returned `released: true` but did not clean git-level metadata (#826)** — when binding was already removed (e.g., agent disbanded), only the daemon worktree pool was checked. Added GC mode for orphan git-level worktrees.
- **`process_server_rate_limit_retries` re-injected `last_input_text` (#841 followup)** — known semantic bug: forces task restart instead of resume. Filed as backlog post-#841 (cfg.prompt switch when bandwidth available). #841 added a complementary recovery nudge that uses `cfg.prompt`.
- **`task action=list` returned 500KB+ in production (#806)** — see Changed (`task action=list` default filter).
- **`cleanup_init_commits` was no-op for daemon-produced heartbeats (#833)** — see Added (trailer-aware body gate).
- **`cleanup_init_commits` retry corruption (#814)** — see Added (auto-recovery from stale rebase-merge dir).
- **CI audit job red on main since 2026-05-15 (PR #831)** — see Changed (audit job permissions).
- **Notification triple-fire on consume + retry (#836)** — see Added (notification dedup race).
- **Test fixture polluted host worktree (#821 prep)** — fixture subprocess `git checkout -b feat-b` landed on the real worktree instead of temp dir. See Added (test isolation invariant).
- **`active:` indicator showed disbanded agents (#827)** — see Changed (filter against runtime registry).
- **WAF-stage / macOS rustup-init transient (#772 v3, PR #800)** — `cache-bin: false` prevents stale rustup-init pollution; v3 supersedes v1/v2 attempts.
- **PTY write timeout prevents chain deadlock (#659, PR #679)** — `PTY_WRITE_TIMEOUT = 5s` saves the supervisor from livelock on a stuck channel.
- **Crash-respawn flakiness (#743, PR #748)** — split `test_crash_respawn_health` predicate into spawn + health-flip phases; reduces test-time variance.
- **`kill_process_tree` PID 0 guard (#681, PR #687)** — refuses to signal PID 0; defensive against accidental kill-all.
- **`flock` protects ci-watch RMW race (#692, PR #731)** — prevents two daemon ticks from clobbering each other.
- **CI lock audit + API connection limits (#711 / #680, PR #730)** — concurrent test-suite serialization + slow-loris connection caps.
- **`fetch --prune` before branch-cleanup remote-gone detection (PR #634)** — `cleanup_merged_branches` could miss squash-merged branches if remote refs were stale.
- **`release_worktree` scope boundary + invariant test (#633, PR #636)** — `release_full` is the single mutation point for binding + worktree cleanup; tests pin no callers bypass.
- **`release_worktree` auto-cleanup merged branches (#611, PR #621)** — landed, then reverted (PR #621), then relanded (PR #621 reland). Final landed.
- **GitHub token format validation (#709, PR #726)** — accepts both `ghp_*` and `gho_*` patterns; rejects malformed.
- **Strip `AGEND_GIT_BYPASS` from child PTY env (#708, PR #725)** — operator's bypass env no longer leaks to agent subprocesses.
- **Default branch + primary remote detection (#690, PR #716)** — handles repos with non-`main` default + non-`origin` primary.
- **Mouse output / SGR / wants_mouse fixes (#700, PR #739 #741 #744)** — three companion PRs cleaning up the mouse forwarding path.
- **Strip trailing `\r` from CI notifications (#719, PR #740)** — was leaking into agent inject.
- **Test isolation — AGEND_GIT_BYPASS in admin + worktree_opt_out (#631, PR #635)** — claim_verifier git calls were rejected without bypass.
- **Cross-team messages bypass ACK absorption (#612, PR #630)** — see §4.5 protocol amendment.
- **GC + reconcile_hooks scan new worktree layout (#682, PR #691)** — post-Sprint-57-Wave-4 layout (worktrees outside repo under `$AGEND_HOME/worktrees/`) was invisible to GC sweep.
- **Exempt orchestrator from ACK absorption skip (#656, PR #666)** — lead-style agents must see ACKs to track team readiness.
- **`replace_instance` backend from fleet.yaml + explicit SPAWN after DELETE (#661 / #662, PR #662 #669)** — respawn race when backend label differed.
- **Startup grace period for `replace_instance` (PR #673)** — newly-replaced instance not classified as `Hung` during boot.
- **`deleted` flag prevents reaper shell fallback race (PR #675)** — `delete_instance` while the agent is mid-reap no longer triggers a fallback shell spawn.
- **PTY inject 2-tier format + atomic header write (#658)** — see Changed.
- **CI watch `ci-stale` lifted to inbox kind (#745, PR #754)** — see Changed.
- **`test_crash_respawn_health` — increase timeout + remove hard sleep (PR #742)** — flaky CI test reliability improvement.
- **Schedule firing race fixed (PR #555-class, via #694 BLOCK 1)** — schedule handler extracted into dedicated `CheckSchedulesHandler`.
- **`fix(verify): remove stale unwrap_used allows (#706, PR #751)** — clippy lint cleanup.
- **`docs(#704): release smoke-test checklist (PR #756)** — operator checklist for release verification.
- **`feat(#704) sub-1: passive PTY capture helper (PR #755)** — sink + rotate + promote CLI for real-backend regression corpus.

### Removed

- **`requires_daemon_state` field (#672 / #674)** — dead field removed; stale CLI hint cleaned.

### Build / dependencies

- **`sysinfo` 0.32 → 0.39** — API migration (`ProcessRefreshKind::new()` → `nothing()`); companion adjustments in `agend-git` shim.
- **`rustls-webpki` upgrade** — security advisory fix paired with audit-job-blocking fix (PR #729).
- **`twilight-model` 0.16 → 0.17.1** (Discord channel dep).
- **`libc` 0.2.184 → 0.2.186**, **`uuid` 1.23.0 → 1.23.1**, **`which` 6 → 8** — routine dependabot bumps.

### Test infrastructure

- **`admin::cleanup_zombies::poll_until_dead` (#934)** — `pub(crate)` deterministic primitive. Polls `kill -0` (Unix) / `OpenProcess` (Windows) every 10 ms up to a timeout; returns `bool`. Replaces sleep-tuned waits in `agent.rs` shutdown + `process.rs` reaper tests.
- **`api::handlers::instance::await_sentinel_nonempty` (#949, rename)** — pre-#949 named for *file existing*, but the instance-boot callers needed *content present*. Renamed to make the contract clear; flake at the four call sites is gone.

### Workflow validation snapshots

- **2026-05-14 post-#779 partial-fix canary pass** — 1 manual git branch step before full bypass-free path. Captured in `/tmp/val-workflow-2026-05-14.md` (post-mortem reference).

## [Workflow validation 2 — 2026-05-14] post #779 partial-fix canary pass (1 manual git branch step)

## [0.6.1] - 2026-05-10

### Removed

- **`agend-terminal mcp` subcommand (Sprint 56 Track I, #531)** — the local-mode stdio JSON-RPC server retired. The `Commands::Mcp` enum variant, `mcp::run` function, ACL machinery, framing helpers, and `proxy_or_local` fallback all deleted from `src/`. Operators with hand-edited mcp.json get the daemon's atomic upsert rewriting their config to use `agend-mcp-bridge` on next start; new installs ship the bridge in release artifacts (Phase 2a, v0.7+). The bridge is the canonical MCP server going forward. Reported by changhansung on Windows 11 + kiro-cli backend; investigated through 4 sequential PRs (Phase 1 RCA / 2a packaging / 2b deprecation / 2c hard removal). See `docs/RCA-issue-531-deprecate-agend-terminal-mcp-2026-05-08.md` for the full architectural reasoning.
- **`ensure_gitignore` worktree helper (#602, #604)** — `src/worktree.rs::ensure_gitignore` auto-injected `.worktrees` into project `.gitignore` as a back-compat backstop for pre-Sprint-57-Wave-4 layouts. Post-Wave-4 worktrees live outside the repo (under `$AGEND_HOME/worktrees/`), making this inject redundant + polluting user `.gitignore`. Removed callsite + helper + obsolete test assert (-42 LOC). Reported by @cheerc.

### Added

- **Bridge runtime invariant (Sprint 56 Track I-Phase2c, #531)** — new `tests/no_local_mcp_mode_invariant.rs::bridge_emits_daemon_error_when_daemon_down` spawns `agend-mcp-bridge` against a clean home with no daemon running and asserts a daemon-related error surfaces in stdout/stderr. Pins the post-removal contract that the bridge has no local-handler fallback path it can silently degrade into.

## [0.6.0] — 2026-05-07

50+ commits since `0.5.0` over Sprint 53 (`agend-git-shim` Phase 1-5 + production wiring) and Sprint 54 (`ci_watch` reliability overhaul + adaptive backoff + agent-visible health surface). Two themes dominate the release: multi-agent git isolation gets its own enforcement layer, and CI feedback to agents gets enough teeth that operators can trust the polling loop.

### Added

- **`agend-git-shim` (Sprint 53)** — five-phase shim layer between agents and `git`. Phase 1: `prepare-commit-msg` hook auto-appends `Agend-Agent`, `Agend-Branch`, `Agend-Issued-At`, `Agend-Task` trailers (idempotent, skipped when present) (#446). Phase 2: shim binary at `$AGEND_HOME/bin/git` with deny matrix on `worktree add/remove/move`, cross-branch `checkout`, and unbound-context ops; bypass via `AGEND_GIT_BYPASS=1` for legitimate operator overrides (#447). Phase 3: per-agent worktree lease/release lifecycle with `.agend-managed` marker (#449). Phase 4: hourly GC dry-run sweep flags stale worktrees without removing them — operator-driven cutover deferred (#454). Phase 5: hotspot detection telemetry for follow-up tuning (#455). Windows in scope (#448).
- **Sprint 53 production wiring** — closes the "Phase 1-5 shipped binaries with no caller" gap from §1.4 hard learning. P0-1 dispatch hook auto-binds and leases on `delegate_task` with branch field (#464). P0-1.5 central lease registry rejects cross-agent branch claim conflicts (#465). P0-1.6 worktree reuse verifies actual HEAD before reusing existing checkout (#466). P0-2 wires `watch_ci` into the dispatch hook (consolidates Hotfix C) (#467). P0-3 anti-pattern CI lint gate enforces `dispatch_auto_bind_lease` is the production code path tests must call (#471). P0-X `release_worktree` MCP tool — single source of truth for binding + worktree cleanup, replaces ad-hoc `binding::unbind` calls (#470). P1-4 `gc_dry_run` MCP tool surfaces Phase 4 GC findings to operators (#479).
- **`ci_watch` multi-caller fan-out (Sprint 54 P0-1)** — `ci watch` MCP action now appends to a `subscribers` array instead of last-write-wins overwrite. Single poll per cycle regardless of subscriber count, terminal classification fans out to all subscribers (no shadow-drop), schema migrates legacy `instance: "X"` to `subscribers: [{instance, subscribed_at}]` with read-fallback for the legacy field. `ci unwatch` removes the caller; deletes the watch file only when subscribers empty. (#484, closes `d-20260506155323776106-0`)
- **`ci_watch` adaptive backoff (Sprint 54 P0-2)** — three-zone curve based on remaining quota: healthy (>50% remaining) uses configured interval, cautious (10–50%) widens 2×, critical (≤10%) widens 4×. Floor at baseline, ceiling at baseline×4. GitHub provider parses `X-RateLimit-Remaining` / `X-RateLimit-Limit` on every successful response; GitLab + Bitbucket emit `None` (preserves baseline behavior). Watch JSON gains `rate_limit_remaining` / `rate_limit_limit` / `effective_interval_secs` diagnostic fields. Recovery path from rate-limit-until reset is unchanged from Sprint 53 (Hotfix F). (#490)
- **GitHub token auto-detect (Sprint 54 P0-4)** — daemon resolves auth via `GITHUB_TOKEN` env → `gh auth token` → unauthenticated fallback. Cached in process-wide `OnceLock`; never written back to env (avoids polluting child PTYs). When neither source yields a token, `ci watch` / `ci status` MCP responses include a canonical `setup_warning` field with actionable text. Daemon restart re-discovers; covers `gh auth login` after daemon was already running. (#487, closes `d-20260506171309264856-1`)
- **Agent-visible CI health surface (Sprint 54 P0-5)** — `ci watch` response gains `rate_limit_active` / `rate_limit_until` / `next_poll_eta` health fields. Daemon fans out `[ci-watch-stalled]` inbox event after 3 consecutive rate-limit skips (exactly once per stall window) and `[ci-watch-resumed]` on the first successful poll afterward — both events go to every subscriber via the P0-1 fan-out contract. New `ci status` MCP action returns caller-scoped 16-field health snapshots with optional `repo` / `branch` filters. (#492)
- **Sprint 54 P1-5 — `cleanup_deployment_dirs` rmdir empty parent** — best-effort `remove_dir` (non-recursive) on the deployment-directory parent after per-member cleanup; preserves operator-dropped files via the non-empty error path. (#489)
- **Sprint 54 P1-7 — `bind_self` MCP tool** — agents self-bind to a fresh worktree on the named branch without going through external dispatch. Reuses the dispatch-hook lifecycle so `binding.json` + worktree + `.agend-managed` marker + auto `watch_ci` registration all land via the same code path. Rejects `main` / `master` (E4.5) and cross-agent branch conflicts. Pair with `release_worktree` to unbind. Solves the recovery case where an agent needs a worktree but has nothing to delegate from. (#493)

### Changed

- **Worktree lifecycle is daemon-managed** — agents no longer call `git worktree add/remove` directly. Auto bind on dispatch (P0-1), audit trail via Phase 1 trailers, exit via the `release_worktree` MCP tool (P0-X). Crashed agents, stale dispatches, and abandoned branches accumulate into the daemon's GC queue rather than as orphan filesystem entries.
- **CI watch architecture split** — the `ci_watch` tick loop separates polling (one HTTP request per cycle, owns rate-limit + adaptive backoff + watch state persistence) from notification fan-out (one inbox enqueue per subscriber after terminal classification). Multi-caller flows that used to last-write-wins now compose cleanly. (#484)
- **`watch_ci` MCP response shape** — `warning` field renamed to canonical `setup_warning` (Sprint 54 P0-4); `subscribers` / `rate_limit_active` / `next_poll_eta` health fields added (P0-5). Pre-Sprint-54 daemons reading post-Sprint-54 watch JSON files still see the legacy `instance` alias for one release cycle.
- **Default PR open mode is `ready`** — implementers no longer open PRs as draft by default. `--draft` is reserved for smoke / verification PRs that won't merge, explicit work-in-progress, and external-PR augmentation. Drafts are hidden from default GitHub UI filters; default-ready keeps the review pipeline visible. (#491)

### Fixed

- **`comms.rs` auto-unbind on `kind=report` reply path (CRITICAL)** — `binding::unbind` was being invoked on every `kind=report` reply, clearing the agent → branch binding even mid-task. Cascade fixed: Phase 1 trailers fire correctly, orphan worktrees no longer accumulate, P0-X release_worktree is no longer a no-op, Phase 4 GC stops false-flagging legitimate live bindings as suspect. The single-mutation-point invariant for `release_worktree` is now load-bearing. (#477)
- **TUI close-path skipped deployment teardown (#474)** — `Ctrl+B x` close on a tab/pane bypassed `cleanup_deployment_dirs`, leaking custom-directory subdirs across daemon restarts. Close path now runs `full_delete_instance` per pane + `reconcile_after_close`. (#475, #481)
- **`ci_watch` malformed head query (Hotfix F gap)** — Hotfix F (#461) closed the `closed_at` freshness gap but the underlying GitHub query was still wrong: `head={branch}` (no owner prefix) is silently dropped by the GitHub API filter, so the response returned the most-recent merged PR *repo-wide* — not the watched branch. Combined with closed_at freshness this manifested as false-positive auto-clear on watches against in-flight PRs. Fix uses the documented `head={owner}:{branch}` form, with a defensive `head.ref` mismatch guard that returns `Unknown` if the response somehow doesn't match. Empirical regression-proof captured (mutate URL back to bare `head=` form → owner-prefix test panics). (#498)
- **`ci_watch` fresh-branch classification fix (Hotfix F)** — daemon was auto-clearing fresh-no-PR branches as `merged=true` because `closed_at` freshness was unchecked. PRs in this state now classify as `pending` and continue polling. Fixed with `closed_at > 1h ago = stale, not auto-clear`. (#461)
- **`ci_watch` no-PR-yet false-positive (Hotfix E)** — branches without a corresponding PR were classified as terminal, dropping notification. 60s grace period + closed_at freshness check. (#458)
- **`agend-git-shim` app-mode wiring missing (Hotfix D)** — `app::run_app` (CLI) didn't initialize the shim init functions, leaving Phase 1-5 ops dead in user-facing CLI. Init seam moved into `bootstrap::prepare` so both daemon and app paths cover the wiring. (#457)
- **`watch_ci` auto-watch on dispatch (Hotfix C)** — `delegate_task` with branch field didn't auto-create a `ci-watch` registration. Wired explicitly; later consolidated into Sprint 53 P0-2. (#451, #467)
- **Server rate-limit retry stores raw body (Hotfix A/B)** — retry loop loses original 429 body across attempts, masking real error messages. Raw body now stored + replayed on inject. Provenance side-channel messages truncated to Telegram length limits to prevent oversize message drop. (#436, #452, #453)
- **Issue #456 deployment teardown cleanup gap** — `deployment teardown` cleared the deployment record but left workspace + configs + channel topic registry behind. Full triple-cleanup (workspace + configs + registry). (#459)
- **Issue #468 — Gemini dismiss patterns substring matched scrollback** — `try_dismiss_dialog` regex matched dialog text inside scrollback buffer, triggering spurious dismissals. Anchored regex with bounded prefix character class. (#469, #472)
- **`reply` MCP `no active channel` silent fallback (#488 — first community-reported issue)** — `reply` consistently returned `no active channel` despite valid Telegram messages. Root cause: MCP subprocess couldn't reach the daemon and silently fell back to the local handler, which lacked `ACTIVE_CHANNEL` registration and surfaced a misleading error. Fix is two-tiered: Tier 1 surfaces `tracing::warn!` at both `proxy_or_local` fallback branches with `tool` / `instance` / `error` fields so future silent fallback is observable; Tier 2 introduces a `requires_daemon_state(tool)` predicate. Tools that touch `ACTIVE_CHANNEL` / `heartbeat_pair` (`reply`, `react`, `download_attachment`) never silently fall back — they return a structured `{"error": "tool '<NAME>' requires daemon API; not reachable: <CAUSE>"}`. Stateless tools (`inbox`, `task`, `list_instances`, `send`) keep the offline-friendly fallback behavior. The `requires_daemon_state` schema field is exposed via `tools/list` so consumers can pre-filter. (#495, thanks @changhansung for the report)
- **Telegram silent drop on image + no-caption + download-fail** — `handle_message` was silently dropping inbox messages when an image arrived without a caption AND its download failed (network / token / size). The user saw the image had been sent; the agent never received the inbox event; no log surfaced the failure. Fix mirrors the #488 silent-fallback pattern: enriched `WARN` log carries `file_id` + `sender_id` + `kind` + `error`; when `is_image && text.is_empty()`, inbox text now reads `[image attached but download failed]`. Captions are never overwritten — user-supplied text always takes precedence. (#497)
- **PTY-inject layer attachment indicator (silent-drop class layer 4)** — `#497` closed the inbound layer (telegram → message store) but a follow-on layer was still dropping the signal: when a pure image with no caption was downloaded successfully and stored in the inbox with `attachments` populated, the PTY-inject formatter (`format_notification_for_inject`) constructed an `[AGEND-MSG]` header with no `attachments=[…]` field and an inline body of empty text — agents reasonably treated this as an accidental empty message. Fix adds two complementary indicators: `pointer_only=true` headers now emit `attachments=[1 photo, 2 document]` in kind-aggregated stable order, and `pointer_only=false` bodies fall back to `[1 photo: cat.jpg]` / `[1 photo attached]` / `[1 photo, 2 document attached]` when text is empty but attachments are present. Filenames come from `original_filename`, not filesystem `path`, so no local-path leakage. New `notify_agent_with_attachments` variant carries the metadata; plain `notify_agent` becomes a thin shim so the three non-telegram callers stay on the old API. Empirical regression-proof captured (mutate `summarize_attachments_for_header` to always return `None` → 3 anchor tests panic with verbatim signatures). (#501)

- **TUI restart input routing** — Pane struct restoration replaced piecemeal field updates that broke input routing on respawn. (#445, thanks @cheerc)
- **Telegram ANSI ESC + typed injection optimization** — strip ANSI escape sequences from outbound, optimize typed injection to prevent ESC conflict. (#462, thanks @cheerc)

### Community

This release includes contributions from external contributors:

- **@cheerc** — #445 (TUI Pane restart routing), #462 (ANSI ESC strip), #473 (fleet.yaml instructions wiring), #474 issue (TUI close path)
- **@changhansung** — first community-reported issue #488 (`reply` MCP no-active-channel)

Thank you for using the project and reporting issues — multi-agent CLI tooling lives or dies on real-world workflows surfacing the gaps.

### Docs

- **FLEET-DEV-PROTOCOL §13 — `AGEND_GIT_BYPASS=1` Usage** — when bypass is required (worktree add/remove on bound paths, daemon-internal git ops), when it isn't (routine operations inside bound worktree pass through cleanly), and the per-scenario hint. (#476)
- **README "Git Behavior Modification" disclosure** — prominent pre-alpha banner section explaining what gets modified (PATH shim, prepare-commit-msg hook, deny matrix, auto bind/lease), why (multi-agent safety, audit trail, lifecycle hygiene, foot-gun guards), risks (agents see different `git`, commits gain trailers, some commands deny unexpectedly, restart needed for shim updates), and bypass paths. (#478)
- **FLEET-DEV-PROTOCOL §7 — PR open semantics** — codifies the default-ready policy + three reserved scenarios for `--draft`. (#491)
- **Sprint 53 PLAN doc + Sprint 54 PLAN doc** — wire-and-cleanup proposal (#463) and reliability+docs sprint proposal (#483, #485 §5.1 amendment, #486 P0-3 absorption note). Public record of the §1.4 hard learning + Path A/C smoke gate classification policy.

### Internal

- **Sprint 53 §1.4 hard learning** — `cargo test green + dual VERIFIED + soak ≠ production wired`. The cushion that caught Sprint 49's deadlock-class regression in pre-IMPL invariants did not catch the dead-code-class regression because no test exercised the actual production entry point (`app::run_app`). Sprint 54 PLAN §5 made production-smoke gates per-phase mandatory; §5.1 carved out parallelizable Path C for non-wiring refactors (`d-20260507004113587226-7`).
- **Empirical regression-proof discipline** (`d-20260506171720519048-2`) — every Tier-2 fix demonstrates that disabling the production change causes a specific test FAIL; restoring it returns to PASS. Captured FAIL signature attaches to the PR description verbatim.
- **`release_worktree` is the single source of truth for binding lifecycle** (`d-20260506171736738779-3`) — all comms.rs handlers treat binding state as read-only; only the dispatch hook (init) and `release_worktree` (exit) mutate. The #477 cascade demonstrated the cost of violating this.
- **Cleanup lifecycle is layered** (`d-20260506171805866878-4`) — three tiers with explicit ownership: per-pane (`full_delete_instance`), per-deployment (`cleanup_deployment_dirs` + `reconcile_after_close`), boot reconcile (`reconcile_orphans`). New cleanup logic must identify which tier owns the new behavior.
- **Fleet IMPL/review dispatch policy** — only `dev` (IMPL) and `reviewer` (review) are dispatchable; `claude-76f359` / `kiro-cli-*` / `gemini-*` are not designated. Lead Path A escalation when dev is unavailable. Captured in lead-side memory after operator m-57 + m-62 corrections.

## [0.5.0] — 2026-05-04

### Added

- **ID-based routing migration (Sprint 46)** — `InstanceId` (UUIDv4) assigned to every fleet instance. Routing resolves through `resolve_instance(name_or_id)` with 3-step resolution (full UUID → short-id → name). Replaces the Sprint 44 M5 name-lookup bandaid. Self-route check compares IDs. Audit trail fields (`emitter_id`, `from_id`, `to_id`) added to task events and dispatch tracking. (#407, #409, #412)
- **CI hardening (Sprint 47)** — Job-level `timeout-minutes: 60` safety net. Per-step timeouts (fmt 5m, clippy 10m, build 20m, tests 20-30m, smoke 10m). Concurrency group with `cancel-in-progress` for PRs — superseded CI runs auto-cancel. (#411)
- **File path migration infrastructure (Sprint 46 P2)** — `inbox_path_resolved` and `metadata_path_resolved` helpers with symlink migration from name-based to id-based paths. (#409)

### Changed

- **Large file split refactor (Sprint 48)** — Three oversized files (~8700 LOC total) split into 25 sub-modules, all ≤700 LOC:
  - `layout.rs` (2170 LOC) → 6 sub-modules: `pane`, `tree`, `preset`, `split`, `tab`, `mod` (#414)
  - `channel/telegram.rs` (4201 LOC) → 13 sub-modules: `state`, `topic_registry`, `send`, `inbound`, `error`, `creds`, `reply`, `bot_api`, `notify`, `adapter`, `ux_sink`, `bootstrap`, `mod` (#416, #419)
  - `render.rs` (2352 LOC) → 7 sub-modules: `core_render`, `border`, `overlay`, `panels`, `panels_fleet`, `scratch`, `mod` (#421)
  - Circular dependency resolved: `split_chunks` moved from render to layout/split (#414)
- **CI workflow cleanup** — Merged redundant clippy/test steps, bumped checkout to v5. (#422)

### Fixed

- **Codex InteractivePrompt false-positive** — Removed codex `Update available!|Press enter to continue` regex that misfired on normal idle prompts, causing spurious operator notifications. (#408)
- **topic_id not persisted on create_instance** — `create_instance` created a Telegram topic but never wrote `topic_id` to `fleet.yaml`. On daemon restart, the topic was orphaned. Now persisted via `update_instance_field`. `describe_instance` also surfaces `topic_id`. (#417, closes #415)
- **Windows CI mock server hang** — Added `Connection: close` header to test mock servers for reliable Windows CI execution. (#420)

### Reverted

- **Sprint 49 channel discipline correction** — Inject-only nudge mechanism (PR #424) reverted due to daemon deadlock and design issues. Follow-up redesign tracked in issue #426. (#425)

### Internal

- Sprint 44 push-time semantic gate: claim verifier + pre-push hook (M1+M2), reviewer SHA gate + ci-watch supersede (M3+M6), hallucinated-fn extension (M4). (#384, #385, #386)
- Sprint 44.5: post-merge rebuild hook + CI slowness investigation. (#388, #389)
- Sprint 45: 15 PRs across 9 architecture groups — persistence/audit, set_var removal, shared runtime, lifecycle, channel, MCP, fleet config, state classifier, CLI/bootstrap. (#390–#404)
- Sprint 48 investigation: bitbucket tests hang under tray feature on Windows — root cause is `tao` Win32 message pump interference, not test logic. (#418)

## [0.4.1] — 2026-04-24

### Fixed

- **`cargo install agend-terminal` build failure on 0.4.0** — `src/protocol.rs` does `include_str!("../docs/FLEET-DEV-PROTOCOL-v1.md")` but the file wasn't in the `Cargo.toml` `include` whitelist. The packaged tarball that `cargo publish` ships to crates.io was therefore missing the bundled protocol doc, and verification compile failed with "No such file or directory". GitHub Release binaries (built from the source tree, not the packaged tarball) were unaffected, so v0.4.0's binary downloads still work — but there is no v0.4.0 on crates.io. v0.4.1 is identical to v0.4.0 in source apart from this single packaging fix.

## [0.4.0] — 2026-04-24

170+ commits since `0.3.2` — multi-agent collaboration plumbing (fleet
protocol v1.1, correlation threads, health reporting), inbox resilience
+ correlation, task-board dependencies + deadlines, a `watch_ci`
reliability overhaul, plus a slate of TUI / spawn lifecycle / Telegram
fixes.

### Added

- **Fleet Development Protocol v1 + v1.1** — `protocol/.default/FLEET-DEV-PROTOCOL.md` formalizes source-of-truth, dispatch contracts, ack absorption, `set_waiting_on` / timeout (§7), and Reviewer Contract addenda (wire-up grep enforcement). Embedded in the binary; extracted to `$AGEND_HOME/protocol/.default/` on first run.
- **Live `<fleet-update>` injection** — daemon broadcasts instance / team / role mutations to every active agent in real time (`fleet_broadcast`); rosters and roles propagate without restart (#113, #123).
- **MCP correlation threads** — `send_to_instance` / `delegate_task` accept `thread_id` + `parent_id` (auto-inherit on reply); new `describe_thread` + `get_thread` MCP tools surface full conversation chains.
- **Health reporting MCP** — agents call `report_health(reason, retry_after?, note?)`; operators clear via `clear_blocked_reason`. `BlockedReason` enum (Hang / RateLimit / QuotaExceeded / AwaitingOperator / PermissionPrompt / Crash) shares a mutex with the hang detector so the two can't race-classify.
- **Daemon watchdog with dry-run mode** — `classify_pty_output` against per-backend fixtures (Claude / Kiro / Codex / Gemini, including a `kiro_false_usage_limit.txt` guard) writes a `BlockedReason` to event_log every tick. `AGEND_WATCHDOG_DRY_RUN=1` keeps writes log-only for a one-week soak before flipping live.
- **Task board: dependencies + deadlines** — `depends_on` auto-blocks downstream tasks until parents are done; new `due_at` + `--duration` field; daemon sweep unclaims overdue tasks back to `open` with event_log + notification.
- **Task-board mutation integrity** — `claim` / `done` / `update` now enforce assignee ownership and target existence; descriptive errors instead of silent no-op (Sprint 5 #4 expanded scope).
- **MCP `target` validation** — `send_to_instance` / `delegate_task` reject unknown instance names instead of enqueueing into a phantom inbox; `delegate_task` resolves teams to orchestrator like `task create` does (#136).
- **Spawn `delivery_mode` field** — `send_to_instance` / `delegate_task` responses distinguish `pty` vs `inbox_fallback` so callers can tell whether the message reached the agent live or just landed in their inbox (#140).
- **Inbox correlation + observability** — `thread_id` / `parent_id` fields, `read_at` + TTL sweep (read 7d / unread 30d soft-delete), `describe_message` MCP tool, schema versioning with future-version rejection.
- **Inbox disk resilience** — readonly mode at <5% free space, atomic append (tmp + fsync + rename), `.draining` half-write recovery, per-file flock covering enqueue / drain / sweep with recovery inside the lock.
- **PTY header injection for large messages** — messages >300 chars (Unicode-aware char count) inject only `[AGEND-MSG] from=X id=Y kind=Z thread=T parent=P size=N` with control-char-sanitized fields; agents drain via `inbox` MCP. Backend instructions teach all four CLIs to recognize the header (with optional ANSI prefix).
- **Idle poll-reminder injection** — daemon notices idle agent + unread inbox > 0 → injects `[AGEND-MSG] kind=poll-reminder unread=N` with atomic dedup so the same count doesn't spam.
- **Schedules: missed one-shot replay** — daemon startup scans `enabled && run_at < now && trigger=once`, fires within 24h window (drops + warns past), wired through the actual fire path with integration test.
- **`watch_ci` reliability overhaul** —
  - `head_sha` tracking + auto-clear when the PR reaches a terminal state (#119, #121)
  - first poll fires immediately after registration instead of waiting `interval_secs`
  - `per_page=5` + `select_runs_to_notify` scans every terminal run since `last_run_id` (rapid-push runs no longer shadowed by an in-progress later run)
  - `classify_runs_response` distinguishes API errors from "no runs" — rate-limit JSON no longer silently drops notifications (#131)
  - background thread errors logged at `warn` instead of silently dropped
  - preventive `warning` field in the `watch_ci` MCP response when `GITHUB_TOKEN` is unset, with `gh auth token` hint (#133)
- **`save_metadata_batch`** — atomic batched write replaces the per-instance loop that produced cross-process write races on Windows CI.
- **Vertical-split mouse resize tolerance** — ±1 column hit zone on the `│` separator so off-by-one clicks no longer fall through to text selection (#139).
- **Split-direction picker** in the MovePaneTarget menu — choose horizontal / vertical when moving a pane into a target tab (#37).
- **TUI usability hints** — `? help` indicator on Task Board overlay; `Ctrl+B ? help` in main status bar (#93, #94).
- **Team-aware peer sections in `agend.md`** — auto-generated peer block distinguishes team members from other fleet agents.
- **Pre-flight Claude session check** — daemon, TUI session-restore, and API spawn paths downgrade `Resume → Fresh` up front when `~/.claude/projects/<encoded-cwd>/` has no jsonl with a `"type":"user"` line. Eliminates the "No conversation found to continue" error flash on idle-pane restart (#130).

### Changed

- **`watch_ci` throttle state in schema** — `last_polled_at` field replaces the mtime-backdating kludge; first-poll-immediate is now a schema-local rule, not a filesystem trick.
- **Crash-respawn uses `Fresh` mode** — stale `--resume` after a crash reliably loops on "conversation not found"; respawn now skips it.
- **`spawn_one` returns the actual `SpawnMode` used** — the Resume → Fresh downgrade is visible to callers so post-spawn gates (e.g. broadcast suppression) act on the real outcome, not the requested mode.
- **`is_tab_bar_row` standalone fn + `TAB_BAR_HEIGHT` const** — eliminates magic `row == 0` checks across mouse + render paths (#38).
- **`spawn_one` uses backend preset `submit_key`** — Gemini's `\n\r` is no longer hardcoded to `\r` (#98).
- **Task IDs gain microsecond + atomic seq** — collision-free under concurrent creates (mirrors `decisions.rs` format).
- **State detection gentler on shells / stuck prompts** — fewer false positives in stall classification (#122).
- **Periodic tick wired into app mode** — schedules, CI watches, health decay, sweeps all run on the same daemon cadence in app mode (previously daemon-only) (#100).

### Fixed

- **`watch_ci` notifies on every terminal CI state**, not just `failure` (#105).
- **Mouse selection white-block residue + Cmd+C false PTY input** (#104).
- **Ctrl+B prefix Shift+key bindings** broken on Kitty-protocol terminals (#100, #102).
- **Shift+Enter newline** + Repeat mode + LF on terminals that need keyboard enhancement disambiguation (#71, #72, #75).
- **`compose_aware_inject` auto-submits when agent is idle** — earlier path required manual submit (#96, #99).
- **Help hint right-align** in status bar (#109).
- **Telegram routing** — channel resolution from passed home (#115); thread home through react / edit / download (#116); topic creation on every API spawn; UxEvent producers wired (#66, #68); block_on-inside-runtime panic prevention (#69); contract test bot-free for macOS (#48).
- **MCP `AGEND_INSTANCE_NAME` injection** into MCP config env for all backends (#61).
- **Stale `ToolUse` / `Thinking` states** expired via periodic tick (#102 follow-up).
- **`delete_instance` guard** only blocks fleet members, not pure ad-hoc instances.
- **Sender identity stamping** via `Sender` newtype — fixes empty `[from:]` header injections.
- **Quickstart `working_directory`** defaults to `$AGEND_HOME/workspace/general`.
- **Tab drag highlight + persistent selection** (#74); 4 UX fixes — Shift+Enter / tab drag reorder / pane drag hit area / single-pane drag (#70).
- **Notify-agent input race** — drop `submit_key` from `notify_agent` (#81).
- **TUI re-tile on team tab initial ingest**.
- **Per-instance workdir for template members**.
- **`AGEND_HOME` env var race** in tests — Windows CI flake source (#107).

### Removed

- **`per_page=1` polling** in watch_ci (replaced with per_page=5 + multi-run scan).
- **Mtime-based throttle state** in watch_ci (replaced with schema field).

### Docs

- **USAGE.md** — startup modes, architecture, keyboard shortcuts (#73).
- **Fleet Development Protocol v1 + v1.1** with §7 Waiting and timeout (#62, #64).
- **Wave 3 Stage B-UX design + Reviewer Contract v0.1** (#50).
- **Track 1 design** — waiting_on annotation + heartbeat (A2 + A5 fix) (#58).

## [0.3.2] — 2026-04-22

Tray-resident arc, Task #9 Option C dual-track elimination,
codebase-review correctness fixes, and performance hotspots.

### Added

- **System tray integration (`agend-terminal tray`, Cargo `--features tray`)** — native menu-bar / system-tray support on all three platforms. Status-keyed icon color (offline / idle / active), 2s status polling, disabled status label at top of menu, "Open App" launches the configured terminal emulator, Autostart (launch-at-login) toggle. Linux ships as an AppImage bundling GTK + AppIndicator libs with a custom AppRun that forces the tray subcommand on launch; macOS + Windows release tarballs include the feature by default.
- **Dual-track fn drift detector** (`tests/no_dual_track_drift.rs`) — integration test scans top-level fn definitions in `src/ops.rs` and `src/mcp/handlers.rs`, panics on body divergence and warns on byte-identical duplicates. Hardened (#31) against raw string literals inside top-level fn bodies (fail-loud; guard scoped to extracted bodies so tests/impl blocks do not false-fail), `extern "C" fn` / `extern "Rust" fn` prefix handling, and silent-drop panic when `match_balanced_brace` cannot close a detected fn.
- **Positive-pin CREATE_TEAM dispatch test** — `RecordingNotifier`-based in-process assertion that `spawn_one` success emits exactly one `ApiEvent::TeamCreated` with the expected payload, completing the three-piece equivalence bracket from C2 and closing a LESSONS-04-21 open item.

### Changed

- **Task #9 Option C — dual-track elimination** — shared helpers consolidated into `src/agent_ops.rs`; `src/api.rs` decomposed into per-tool handler modules (`handlers/instance.rs`, `handlers/team.rs`, `handlers/*`); `src/ops.rs` reduced to a single `start_instance` wrapper (Task #12 then deleted it entirely, inlining into the MCP dispatcher); 21 dead CLI-wrapper fns pruned and the crate-level `#![allow(dead_code)]` attribute removed. `validate_branch` also migrated out of `src/worktree.rs` into `agent_ops`.
- **MCP tool ACL cached via `OnceLock`** — parsed once at startup instead of on every tool call.
- **Layout pane-id enumeration** — new `collect_pane_ids()` avoids recursive allocation in the layout traversal hotspot.
- **`spawn_agent` decomposed** — `build_command()` extracted for clarity and unit-testability.

### Fixed

- **Invalid state regex now panics** instead of silently degrading state detection.
- **`strip_ansi` no longer inserts a phantom space on cursor-move sequences**, which was corrupting captured output.
- **MCP stdio framing** returns `None` on EOF during `Content-Length` error recovery instead of hanging the loop.
- **Cron schedule robustness** — `parse_run_at` rejects invalid timezones (previously fell back to UTC); schedule skipped instead of mis-fired on bad tz; `.schedule_last_check` written atomically.
- **Fleet `ready_pattern` hardening** — regex validated at resolve time with a size ceiling, closing a ReDoS surface.
- **Tray "Open App" no longer freezes the tray** — terminal launches are detached.

## [0.3.1] — 2026-04-21

Substantial work has landed on `main` since `0.3.0`. Highlights, grouped by area.

### Added

- **Terminal app (`agend-terminal app` / no-arg launch)** — multi-tab, multi-pane TUI that spawns and attaches agents in-process. Tab per agent, nested splits, joined pane borders, layout presets (`Ctrl+B Space`), zoom, rename, scroll mode, command palette, decisions / tasks overlays. Session layout persists via reconciliation against `fleet.yaml`.
- **Tmux-style keybinds** (`Ctrl+B` prefix): `c n p l 0-9 & , . w " % o x z [ d ?` plus repeat mode.
- **Pane interaction** — drag to swap panes, resize with arrow keys, mouse scroll per pane, selection + clipboard (`arboard`), IME-aware cursor.
- **Auto tab/pane for MCP-spawned instances** — `create_instance` / `create_team` from an agent automatically surfaces new panes in the TUI.
- **Windows-support Phase A** — `nix` dependency removed; file locks via `fs2`, PID helpers via `src/process.rs` with platform-conditional `libc` / `windows-sys` impls; `/tmp` hardcoding replaced with `dirs::home_dir()` / `std::env::temp_dir()`. UDS remains the last Windows blocker (see `docs/archived/PLAN-windows-support.md`).
- **Connect command (`agend-terminal connect`)** — dynamically register an external agent with the running daemon (inbox-only, no PTY management) for headless environments.
- **Telegram in app mode** — status-bar connection indicator, notification routing to the owning pane.
- **CI & release workflow** — artifact uploads, per-platform builds.

### Changed

- **Single source of truth for fleet** — `fleet.yaml` holds agent definitions, `session.json` holds pure layout. Session reconciles against fleet on startup.
- **Unified daemon** — `DaemonCore` shared between standalone daemon and in-process app; API server + MCP tools available in both modes.
- **Logging** — all `eprintln!` migrated to `tracing`; log timestamps use local timezone.
- **Agent instructions** — auto-written `agend.md` covers identity / role / peers / MCP tool usage; per-backend variants (`.claude/rules/agend.md`, `.kiro/steering/agend.md`, `AGENTS.md`, `GEMINI.md`, `.codex/config.toml`).
- **Backend aliases** — `kiro` accepted as alias for `kiro-cli`, etc.; serde aliases prevent fleet.yaml breakage.
- **Code review follow-ups** — multiple rounds of hardening landed: mutex poison recovery unified, split-fallback no longer leaks forwarder threads, team handler instructions pre-generated, layout hint parsing renamed (`from_str` → `parse_hint`), overlay bounds fixed under clippy 1.95.
- **Drag / resize hardening** — drag borders disambiguated from state colors; tmux-style resize direction; split-ratio bounds scale with cell count; Unicode width used for title hit-testing.
- **Mouse event routing** — overlay modal, drag guard, zoom gating; mouse clicks no longer switch panes while zoomed.

### Fixed

- Shutdown cleanly drains the registry before killing processes.
- Delete-instance removes working directory, metadata, session entry, Telegram topic.
- Respawn preserves `--mcp-config` and `--settings` flags; uses `fresh_args` to stop resume crash loops.
- Codex trust directory prompt auto-dismissed; `.codex/config.toml` scoped per-project.
- Claude Code receives `--mcp-config` so it picks up the agend MCP server.
- Orphaned Telegram topics cleaned up on daemon restart.
- Worktree creation handles empty-repo + set-git-config edge cases.
- Bugreport redacts `group_id`.
- Various clippy 1.95 fixes (`collapsible_if`, `type_complexity`, `unwrap_used`, overlay bounds match guards).
- **Unique instance names on every spawn** — 6-hex suffix against `fleet.yaml` ∪ `workspace/` ∪ `inbox/`; pane close cleans up the workspace and inbox entry so the next spawn cannot accidentally resume a stale agent session.
- **Codex trust prompt auto-dismiss now works on macOS** — dismiss pattern matching runs against the VTerm-rendered screen (not a hand-rolled strip_ansi over raw bytes), so Ink-style char-by-char cursor-positioned paints still match. Codex dismiss key switched from LF to CR — macOS/openpty does not translate LF→CR on input like ConPTY does, so LF was silently acting as Ctrl+J (move selection down).

### Removed

- Obsolete stale debug blocks; `workspaces/` → `workspace/` directory rename; `agend-terminal agent` CLI subcommand (agents now use MCP, not CLI, for inter-agent comms).

---

## [0.3.0] — 2026-03 (tag: `release: v0.3.0`)

> Commit `85f2bc3` — "release: v0.3.0 — fleet orchestration + stability"

### Added

- **Fleet orchestration** — `fleet.yaml` as first-class config, Telegram topic persistence for dynamic instances, `fleet.yaml` single-source reconciliation.
- **MCP tool surface** — 35 tools across user comms, agent comms, instance lifecycle, decisions, tasks, teams, schedules, deployments, repo sharing. MCP socket pooling (proxy via daemon).
- **Quickstart wizard** (`agend-terminal quickstart`) — interactive setup, handles existing `fleet.yaml` / `.env`.
- **Demo command** (`agend-terminal demo`) — split-screen live conversation with crash recovery.
- **Bugreport command** — one-file diagnostic export.
- **Git worktree isolation** — `src/worktree.rs`, auto per-agent worktree; original repo untouched.
- **Structured logging** via `tracing`.
- **Protocol versioning** + fleet snapshot.
- **CI-loop** — auto-watch GitHub Actions and inject failure logs into agents.
- **Friendly errors**, `--json` output, shell completions.
- **Telegram integration** — topic-per-instance routing, crash notifications.

### Changed

- File locking via `flock` (auto-release on crash).
- `AGEND_TERMINAL_HOME` → `AGEND_HOME`, default dir `~/.agend`.
- `create_instance` applies backend preset args (e.g. `--yolo`, `--dangerously-skip-permissions`).
- Tokio features slimmed (`full` → `rt,net,io-util,fs,macros,time`).
- Input sanitization for instance names, branch names, paths, download filenames.

### Fixed

- Respawn on exit code 0 (daemon-managed agents should not silently disappear).
- MCP config format corrected across all 5 backends.
- Delete flow: no spurious crash logs; clears stale session ID.
- Shutdown flag distinguishes crash from daemon stop; suppresses crash handling during `Ctrl+C`.
- Attach uses daemon's run directory, not CLI process PID.
- Preserve `HealthTracker` across respawns.
- Mouse scroll support in attach mode.

### Baseline

- PTY ownership via `portable-pty`; `alacritty_terminal` for VTerm.
- `ratatui` + `crossterm` for TUI.
- Unix-only at release time; Windows support is post-0.3.0 in-progress (Phase A landed on `main`).
- Backends: Claude Code, Kiro CLI, Codex, OpenCode, Gemini CLI.

---

[0.8.0]: https://github.com/suzuke/agend-terminal/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/suzuke/agend-terminal/compare/v0.6.1...v0.7.0
[0.6.1]: https://github.com/suzuke/agend-terminal/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/suzuke/agend-terminal/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/suzuke/agend-terminal/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/suzuke/agend-terminal/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/suzuke/agend-terminal/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/suzuke/agend-terminal/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/suzuke/agend-terminal/compare/85f2bc3...v0.3.1
[0.3.0]: https://github.com/suzuke/agend-terminal/commit/85f2bc3
