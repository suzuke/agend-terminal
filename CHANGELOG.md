# Changelog

All notable changes to this project are documented here.
Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); project follows [SemVer](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Post-`0.3.1` work on `main`. 48 commits over the tray-resident arc,
Task #9 Option C dual-track elimination, codebase-review correctness
fixes, and performance hotspots.

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

[Unreleased]: https://github.com/suzuke/agend-terminal/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/suzuke/agend-terminal/compare/85f2bc3...v0.3.1
[0.3.0]: https://github.com/suzuke/agend-terminal/commit/85f2bc3
