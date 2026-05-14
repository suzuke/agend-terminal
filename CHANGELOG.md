# Changelog

All notable changes to this project are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); project follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/suzuke/agend-terminal/compare/v0.4.1...HEAD
[0.4.1]: https://github.com/suzuke/agend-terminal/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/suzuke/agend-terminal/compare/v0.3.2...v0.4.0
[0.3.2]: https://github.com/suzuke/agend-terminal/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/suzuke/agend-terminal/compare/85f2bc3...v0.3.1
[0.3.0]: https://github.com/suzuke/agend-terminal/commit/85f2bc3
