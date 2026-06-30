# Deep Audit 2026-06-29 — Phase 1: Canonical Feature Inventory

> Code-level inventory of **every user-facing surface** (~250 features across 6 subsystems),
> cross-referenced against `docs/` (the intended-behaviour spec). This complements the prior
> `agend-terminal-user-story-feature-tracker.xlsx` (107 doc-driven stories) by going down to the
> handler / keybinding / service level and linking each area to confirmed audit findings.
>
> **Column mapping to the requested canonical schema:** *Feature ID* · *Feature* (= Feature Name) ·
> *Expected* (= Description + Expected Behaviour, condensed) · *Test* (= Test Status) ·
> *Finding* (= Related Issues → `AUDIT2-NNN` in `DEEP-AUDIT-2026-06-29-ISSUES.md`, or `—` = behaves
> as specified) · *Notes*. Realistic **User Stories** live in `DEEP-AUDIT-2026-06-29-USER-STORIES.md`.
> *Current Behaviour* = **matches Expected unless a Finding is linked.**

Legend — Test: ✅ tested · ◑ partial · M manual-only · ❌ none.

---

## 1. CLI surface (`agend-terminal`, `agend-git`, `agend-mcp-bridge`)

| ID | Feature | Expected | Test | Finding | Notes |
|----|---------|----------|------|---------|-------|
| CLI-01 | `start [--foreground] [--fleet]` | Start daemon from fleet.yaml; detached by default | ◑ | AUDIT2-018 | docs say `--detached` (renamed) |
| CLI-02 | `start --agents <name:cmd>...` | Ad-hoc fleet, no fleet.yaml; implies foreground | ❌ | — | subsumes old `daemon` |
| CLI-03 | `app [--fleet]` | Launch multi-pane TUI; attach or own fleet | ✅ | — | non-TTY guard fixed (ERR-007) |
| CLI-04 | `attach [name]` | Attach to one agent PTY; `Ctrl+B d` detaches | ✅ | — | daemon-offline hint |
| CLI-05 | `inject <name> <text>` | Send text to agent PTY (manual `\r`) | ❌ | — | |
| CLI-06 | `list/ls/status [--detailed][--json]` | List agents; plain offline, rich via API | ✅ | — | #938 JSON mode field |
| CLI-09 | `connect <name> --backend ...` | Register external (inbox-only) agent | ✅ | — | dereg on spawn fail (ERR-004) |
| CLI-10 | `kill <name>` | Stop one agent; daemon may respawn | ❌ | — | |
| CLI-11 | `stop` | Shut daemon + all agents | ❌ | — | |
| CLI-12 | `mode <mode> [--delegate][--scope]` | Set operator availability (active/away/sleep) | ❌ | — | #1339; client-side validation absent |
| CLI-13 | `admin cleanup-branches [--yes]` | Squash-safe local branch GC; dry-run default | ❌ | — | |
| CLI-14 | `admin cleanup-zombies [--age][--yes]` | Reap stale daemons; SIGTERM→SIGKILL | ❌ | — | #927 |
| CLI-15 | `capture backend --backend [--seconds]` | Dump backend VTerm screen | ❌ | — | state-detection debugging |
| CLI-16 | `capture promote <path> <scenario> ...` | Promote .cap → fixture + manifest | ❌ | — | orphan-on-missing-manifest fixed (ERR-005) |
| CLI-17 | `verify [--json][--backend][--quick]` | E2E/in-process probes <30s | ❌ | — | instructions probe fixed (ERR-001) |
| CLI-18 | `service install/uninstall/status` | User-level OS service mgmt; idempotent | ❌ | — | systemd/launchd/Task Scheduler |
| CLI-21 | `doctor [--format]` | Validate home/.env/fleet/sockets/backends | ❌ | — | mtime helper-staleness heuristic |
| CLI-22 | `doctor providers [--probe]` | Fugu/Sakana detection for codex | ❌ | — | |
| CLI-23 | `doctor topics [--cleanup][--yes]` | Telegram topic live/orphan classify | ❌ | — | needs can_manage_topics |
| CLI-24 | `skills add/remove/list/update/install` | Unified cross-backend skill mgmt | ◑ (RUN-006) | AUDIT2-013 | concurrent same-allowlist stage race |
| CLI-29 | `tray` | Menu-bar resident (feature-gated) | ❌ | — | `--features tray` only |
| CLI-30 | `quickstart [--unattended]` | Onboarding; detect backends, Telegram, fleet | ❌ | — | idempotent re-run |
| CLI-31 | `bugreport` | Redacted diagnostics bundle | ✅ | — | writes under AGEND_HOME (ERR-002) |
| CLI-32 | `completions <shell>` | clap_complete scripts | ✅ | — | |
| CLI-33 | `verify-push --base [--head] --claim...` | Verify push claims vs diff | ❌ | — | claim_verifier |
| CLI-34 | `hook-event --instance [--event]` | Hidden backend lifecycle reporter; always exit 0 | ❌ | — | #2413 Shadow Observer |
| BIN-01 | `agend-mcp-bridge` | MCP JSON-RPC stdio ↔ daemon API | ❌ | — | #1000 content-dedup 500ms |
| BIN-02 | `agend-git` | Git PATH shim: route/passthrough/deny | ✅ | — | #1504 recursion, #2234 canonical-HEAD deny |
| — | **doc-only** `demo`/`upgrade`/`fleet`/`daemon`/`test` | — | — | AUDIT2-018 | documented, not implemented |

## 2. MCP coordination tools (37 tools / 77 action variants)

| ID | Feature | Expected | Test | Finding | Notes |
|----|---------|----------|------|---------|-------|
| MCP-01 | `reply` | Post to operator channel; optional timeout default-action | ✅ | — | |
| MCP-02 | `download_attachment` | Fetch Telegram media by file_id | ✅ | — | |
| MCP-03 | `send` | Unified send; kind=task requires task_id | ✅ | AUDIT2-011 | auto-create id collision |
| MCP-04..08 | `inbox` drain/ack/clear/message_id/thread_id | Read/settle inbox; dedup retry | ✅ | — | #2299 ack |
| MCP-09 | `list_instances` | Compact/verbose instance table | ✅ | — | #2475 |
| MCP-10..15 | `create/delete/start/replace/restart_instance` | Lifecycle; restart resume/fresh w/ dirty gate | ✅ | AUDIT2-002, AUDIT2-004 | delete_instance no caller ACL; env injection |
| MCP-16 | `interrupt` | ESC to PTY; optional snapshot | ✅ | — | |
| MCP-17..21 | `set_display_name/description/waiting_on/move_pane/pane_snapshot` | Pane metadata + scrollback read | ✅ | — | #2478 to_file captures (no expiry) |
| MCP-22 | `tui_screenshot` | TUI→SVG (TUI mode only) | ✅ | — | |
| MCP-23..26 | `decision` post/list/update/answer | Decision board + async questions | ✅ | — | author daemon-derived (ACL safe) |
| MCP-27..37 | `task` create/list/get/claim/done/update/sweep/health/activity/metadata_* | Task board | ✅ | AUDIT2-005, AUDIT2-014 | metadata unbounded; cross-board dep race |
| MCP-38 | `task_sweep_config` | Auto-close daemon config (GH PR merge) | ✅ | AUDIT2-001 | api_base_url same SSRF vector |
| MCP-39..42 | `team` create/delete/list/update | Team roster; one-agent-one-team | ✅ | — | partial-cascade on delete (uncertain) |
| MCP-43..46 | `schedule` create/list/update/delete | Cron + one-shot dispatch | ✅ | AUDIT2-010 | cron DST mis/double-fire |
| MCP-47..49 | `deployment` deploy/teardown/list | Template team spawn; teardown needs `name` | ✅ | — | teardown name required (refuted ambiguity) |
| MCP-50..52 | `ephemeral` spawn/list/reap | Short-lived workers; reaped by TTL | ✅ | — | gated AGEND_EPHEMERAL_REAL_BACKEND |
| MCP-53..55 | `ci` watch/unwatch/status | Poll GH/GitLab/Bitbucket CI | ✅ | AUDIT2-001, AUDIT2-009 | SSRF; rerun-green swallowed |
| MCP-56..57 | `health` report/clear | Blocked-reason for supervisor | ✅ | — | |
| MCP-58..61 | `watchdog` snooze/resume/status/ack | Suppress idle alerts | ✅ | — | overflow→silent 1h fallback |
| MCP-62..64 | `config` get/set/list | Runtime config (allowlisted) | ✅ | AUDIT2-003 | sensitive gates agent-settable |
| MCP-65..69 | `repo` checkout/release/cleanup_*/merge | Worktree + branch + PR merge | ✅ | AUDIT2-002 | merge no caller ACL |
| MCP-70..74 | `bind_self/release_worktree/force_release_worktree/binding_state/gc_dry_run` | Daemon worktree binding | ✅ | AUDIT2-002 | force_release no ownership check |
| MCP-75 | `tokens` | Usage/cost estimate (time-window attribution) | ✅ | — | cost is ESTIMATE |
| MCP-76 | `mode get` | Read operator mode (agents read-only) | ✅ | — | |
| MCP-77 | `restart_daemon` | Graceful self-respawn; legacy exit(42) | ✅ | — | restart race mitigated (refuted) |

## 3. TUI / interactive surface (52 features)

| ID | Feature | Expected | Test | Finding | Notes |
|----|---------|----------|------|---------|-------|
| TUI-1..8 | Tab mgmt (`c/n/p/l/0-9/&/,/w`) | New/next/prev/last/goto/close/rename/list | ✅/M | AUDIT2-016 | close_tab focus mis-route on left-removal |
| TUI-9..17 | Pane mgmt (`"/%/o/arrows/x/z/@/!/.`) | Split/focus/close/zoom/flip/move/rename | ✅/M | — | focus_id reassign safe (refuted) |
| TUI-18..22 | Resize + layout cycle (Alt/Shift+HJKL, Space) | 5% resize; 5 layout presets | ◑ | — | dual Kitty/legacy encoding |
| TUI-23..26 | Scroll mode (`[`, j/k, PgUp/Dn, q/Esc) | Scrollback overlay | M | AUDIT2-017 | offset unclamped on history shrink |
| TUI-27..31 | Selection + copy (Shift-drag, Cmd/Ctrl+Shift+C, `e`, wheel) | Select/copy/copy-mode | ✅/M | — | selection drop-on-close safe (refuted) |
| TUI-32 | Image paste (`Ctrl+B i`) | Clipboard image → marker inject | ✅ | — | #2443 off-by-one refuted |
| TUI-33..39 | Overlays (`:`, D, T, S, M/F, ~, ?) | Palette/decisions/tasks/status/monitor/scratch/help | ✅/M | — | palette bounds safe (refuted) |
| TUI-40..52 | Mouse (click/drag/wheel/SGR-forward), Ctrl+B Ctrl+B, detach | Focus/reorder/resize/forward/detach | ✅/M | — | resize-mid-drag safe (refuted) |

## 4. Daemon background services (55)

| ID | Feature | Expected | Test | Finding | Notes |
|----|---------|----------|------|---------|-------|
| DMN-01..03 | Supervisor / crash-respawn / health state machine | 10s tick; backoff 5s→5min; Healthy→…→Failed/Paused | ✅ | AUDIT2-007 | crash arm not panic-isolated |
| DMN-04..07 | Hang detection + recovery stages 1-3 | Silence classify; ESC→restart→Pause; shadow default | ✅ | AUDIT2-008 | Stage-2 notify storm; Failed-wedge (uncertain) |
| DMN-08..10 | CI watch / conflict / PR-state aggregation | Adaptive poll; mergeable; pr-ready join | ✅/◑ | AUDIT2-009 | rerun-green swallowed (multi-workflow) |
| DMN-11..12 | Dispatch-idle L1/L2 | Timeout + nudge escalation | ✅ | — | #2031 |
| DMN-13..14 | Schedules cron / one-shot | TZ-locked; ≤24h replay | ✅ | AUDIT2-010 | cron DST; one-shot safe (refuted) |
| DMN-15 | Deployments | Template spawn + worktrees + team | ✅ (integration) | — | |
| DMN-16..24 | Watchdogs (PTY/idle/anti-stall/decision/conflict/helper/waiting-on/registry) | Pattern + idle + stall alerts | ◑ | — | various 5-min throttles |
| DMN-25..26 | Auto-release / canonical-drift | gh merge --auto; fleet↔manifest drift | ✅/◑ | — | |
| DMN-27..34 | Retention / inbox maint / progress mirror+backstop / notif flush | GC + exfil-gated relay | ◑ | AUDIT2-003 | progress_mode=1 exfil surface |
| DMN-35..49 | Respawn/snapshot/shadow-observe/liveness/GC ticks/handoff/reclaim | Periodic hygiene + recovery | ◑ | — | |
| DMN-50 | Instance monitor | RSS/CPU 5s tick (TUI-gated) | ◑ | — | |
| DMN-51..52 | Notification queue / event bus | Draft-state defer; pub/sub dispatch | ✅ | AUDIT2-006 | bus blocks tick thread |
| DMN-53..55 | Boot-sweep / run-dir discovery / daemon restart | Zombie cleanup; PID+token identity | ✅ (integration) | AUDIT2-018(PID note) | PID-only fallback on unreadable token |

## 5. Config / channels / backends / fleet (CFG / CHAN / QS / BACKEND / FLEET / ENV)

| ID | Feature | Expected | Test | Finding | Notes |
|----|---------|----------|------|---------|-------|
| CFG-01..08 | AGEND_HOME / .env / fleet.yaml / runtime-config / mcp-config / sensitive-env / DaemonConfig / TZ | Layered config; daemon-vs-operator merge | ◑ | AUDIT2-004, AUDIT2-012 | env deny-list gap; runtime-config non-atomic |
| CHAN-01..07 | Telegram channel (trait, token indirection, supergroup, topics.json, dedup, allowlist, fleet-binding) | Platform-neutral channel; fail-closed allowlist | ❌ (scaffold) | — | topics.json drift risk (uncertain) |
| CHAN-08 | Discord channel | Feature-gated placeholder | ❌ | — | `--features discord` |
| QS-01..08 | Quickstart steps (detect/token/verify/group/save/fleet/unattended/resume) | Onboarding flows | ❌ | — | 3-min poll hard-coded |
| BACKEND-01..10 | claude/kiro/codex/opencode/agy/shell/raw + cred-isolation/hooks/red-anchor | 5 supported backends + fallbacks | ❌ | — | hook-state phase-1 only |
| FLEET-01..13 | Startup/resume/foreground/--agents/merge/skills-allowlist/topic-bind/model-tiers/env-merge/ready/worktree/watchdog | Fleet lifecycle + config | ❌ | — | merge-classification exhaustiveness (uncertain) |
| ENV-01..10 | Bot tokens, HOOK_STATE_POC, PROGRESS_MODE, ENV_ISOLATION, WORKTREE_GC, AUTO_RECOVERY, SUPERVISED/WRAPPED/HANDOFF, POINTER_ONLY_INJECT | Env kill-switches / gates | ❌ | AUDIT2-020 | `AGEND_TURN_SENTINEL_SHADOW` is dead |

## 6. Core coordination domain (34)

| ID | Feature | Expected | Test | Finding | Notes |
|----|---------|----------|------|---------|-------|
| CORE-01..12 | Task event log v2 + create/list/claim/done/update/health/sweep/deps/ACL/lifecycle/metadata | Append-only JSONL; auto-block; ACL | ✅ (30+) | AUDIT2-005, AUDIT2-011, AUDIT2-014 | compaction safe (refuted) |
| CORE-13..16 | Teams CRUD / orchestrator routing / degradation / broadcast | Named groups; one-agent-one-team | ❌ | — | partial-cascade delete (uncertain) |
| CORE-17..20 | Decisions storage/post-list/versioning-TTL/async-board | Per-file ledger; TTL 90d | ❌ | — | author ACL safe (refuted) |
| CORE-21..25 | Inbox storage / delivery modes / threading / reply-ledger escalation / message kinds | Per-agent JSONL; reply obligations | ✅ | AUDIT2-015 | reply dedup not persisted (by-design); fsync gap |
| CORE-26..30 | Worktree create/lease/release-GC/auto-cleanup/binding | Per-agent git isolation | ❌ | AUDIT2-002 | release atomicity safe (refuted); force_release ACL |
| CORE-31..32 | Skills install/discovery + CRUD | Unified source → backend symlinks | ◑ | AUDIT2-013 | stage race |
| CORE-33..34 | Token cost (Claude / Codex) | Per-instance usage estimate | ❌ | — | prices hardcoded; estimate only |

---

## Coverage & test-status summary
- **Strongest test coverage:** task board / event log (30+), MCP handlers, inbox, agent lifecycle,
  daemon supervisor/restart (integration). **Weakest:** channels (trait scaffold, no live tests),
  quickstart/onboarding, backends, fleet merge, decisions, teams, token cost — almost entirely
  untested at unit level (manual RUN-001..020 in the prior xlsx only).
- **Most finding-dense subsystems:** MCP tool authorization (Group A), daemon tick reliability
  (Group B/C), state-file atomicity (Group D).
- **Behaving as specified (no finding):** the large majority of features — and notably everything in
  the **§ Refuted** list of `DEEP-AUDIT-2026-06-29-ISSUES.md`, where the code defends correctly
  against a plausible-looking failure mode.
