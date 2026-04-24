# Changelog

All notable changes to this project are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); project follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
