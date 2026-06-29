# Deep Audit 2026-06-29 — Phase 2: Adversarial User Stories

> Second-pass independent audit (deeper than PR #2507 / `agend-terminal-user-story-feature-tracker.xlsx`,
> which captured 107 happy-path stories). This document focuses on **edge cases, invalid input,
> interrupted/cancelled/repeated/async/multi-step workflows, error recovery, navigation, and
> deliberately-hostile user behaviour** — the scenarios most likely to surface defects. Each story
> links to a Feature ID from the inventory and (where it produced a finding) an Issue ID in
> `DEEP-AUDIT-2026-06-29-ISSUES.md`.

Scenario classes per the audit brief: **N**ormal · **E**dge · **I**nvalid · **X**interrupted ·
**R**epeated · **A**sync · **M**ulti-step · **C**ancellation · **V**navigation · **U**nexpected.

---

## 1. CLI & daemon lifecycle (CLI-01..14)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-CLI-1 | N | As an operator I run `start` then `app` to bring the fleet up and attach. | Detached-vs-foreground default changed (#2507 era); docs say `--detached`, code says `--foreground`. |
| S-CLI-2 | I | I run `agend-terminal demo` / `upgrade` / `fleet start` because the docs list them. | Commands are documented but absent from the clap enum → "unrecognized subcommand" (confirmed drift). |
| S-CLI-3 | X | I `Ctrl+C` `start` while agents are still being spawned (stagger window). | Partial fleet up; some PTYs spawned, fleet.yaml half-merged; is cleanup idempotent? |
| S-CLI-4 | R | I run `start` twice (daemon already running) / `service install` twice. | Idempotency claims — second daemon must refuse via lock, not double-spawn. |
| S-CLI-5 | E | I run `connect badname --backend /no/such/bin`. | ERR-004 (prior): external agent registered before spawn → stale state. Re-verify the fix holds for other failure points. |
| S-CLI-6 | U | I run `kill <agent>` repeatedly, or `kill` a name that doesn't exist. | Respawn may immediately bring it back (HealthTracker); unknown name should error cleanly. |
| S-CLI-7 | I | `admin cleanup-zombies --age garbage` / `--age -5d`. | Duration parse failure path; negative/zero age. |
| S-CLI-8 | C | I answer `n` to `cleanup-branches` / `cleanup-zombies` confirmation. | Must be a true no-op with no deletions. |
| S-CLI-9 | M | `capture backend` → `capture promote` → fixture replay. | ERR-005 (prior): promote left orphan state on missing manifest. Re-verify. |
| S-CLI-10 | U | I run `app` with stdin/stdout piped (no TTY) / inside CI. | ERR-007 (prior): unconditional `ratatui::init()` panicked + corrupted terminal. Re-verify guard. |

## 2. MCP coordination tools (MCP-01..77)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-MCP-1 | I | An agent calls `send kind=task` **without** `task_id`. | Anti-stall contract requires task_id; is it rejected at schema or silently accepted/late-failed? |
| S-MCP-2 | U | An agent is spawned with a **mistyped role** (`role: revewer`). | Role→tool-subset may FAIL-OPEN (grant all 37 tools) instead of denying. Security-relevant. |
| S-MCP-3 | I | `ci watch` with `ci_provider_url: http://attacker.internal/`. | No URL validation → daemon makes authenticated requests to arbitrary host (SSRF). |
| S-MCP-4 | I | `config set` a sensitive key (e.g. a recovery gate or `progress_mode=1`). | Keys auto-derived from serialization; no allowlist → remote toggle of exfil/safety gates. |
| S-MCP-5 | A | Two agents `claim` the same task in the same tick. | Append-only race; last-writer / dependency memoization window. |
| S-MCP-6 | R | An agent re-sends an identical message (LLM double-fire) within the dedup window. | Bridge 500ms + channel 5s dedup; restart drops in-memory dedup. |
| S-MCP-7 | E | `task done` with `force=true` on a ghost-owned task. | Force path audit trail is event-log-only, not surfaced in MCP response. |
| S-MCP-8 | C | `interrupt` an agent mid-tool-use, then immediately `restart_instance mode=fresh` on a dirty worktree. | Dirty-worktree safety gate vs force bypass; uncommitted work loss. |
| S-MCP-9 | U | Call `task action=activity` (declared action). | Possibly declared-but-unimplemented → error or silent no-op. |
| S-MCP-10 | M | `deployment deploy` template → `deployment teardown` when two deployments exist. | teardown takes no id → ambiguous target. |
| S-MCP-11 | U | `task metadata_set` with a 10MB JSON value. | No size/type guard → list/get bloat / DoS. |
| S-MCP-12 | E | `decision update` a decision asserting a different `author`. | ACL trusts author field — can it be spoofed by the caller? |

## 3. TUI interaction (TUI-1..52)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-TUI-1 | X | I close the focused pane/tab while output is streaming. | focus_id orphaning → stale index panic or input routed to wrong pane. |
| S-TUI-2 | E | I scroll up 10k+ lines, keep the agent producing output until scrollback evicts. | scroll_offset points into evicted region → frozen/blank view. |
| S-TUI-3 | X | I start a text selection, then the pane is removed (agent exits). | Selection state persists referencing a dead pane. |
| S-TUI-4 | U | At the close-confirmation prompt I press Space / arrow / Enter. | Any non-`y` key dismisses (or worse) → accidental cancel/confirm. |
| S-TUI-5 | R | I press `Ctrl+B i` (image paste) rapidly many times. | Temp-file cleanup TTL (SystemTime) may delete a not-yet-read file; #2443 off-by-one. |
| S-TUI-6 | V | `Ctrl+B 9` to a non-existent tab; directional focus into empty space. | Silent no-op vs panic on out-of-range index. |
| S-TUI-7 | I | Rename a tab/pane to empty string or 5000 chars. | Empty accepted (no validation); width overflow in tab bar render. |
| S-TUI-8 | A | Terminal is resized while I'm dragging a split border. | Cached drag rect vs new area → resize math overshoot. |
| S-TUI-9 | M | Open command palette, type partial, Tab-cycle completions past the visible window. | selected index can exceed visible_count. |
| S-TUI-10 | U | I run inside a non-TTY / very small (1×1) terminal. | Layout math with zero/one cell; init guards. |

## 4. Worktree isolation & git (CORE-26..30, BIN-02)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-WT-1 | A | Two agents lease/release worktrees on the same repo concurrently. | released_at write not under the binding lock → lost update / double-release. |
| S-WT-2 | X | An agent crashes after lease but before release. | orphan_reconcile_leases is log-only → stale binding.json + leaked worktree dir. |
| S-WT-3 | E | `bind_self branch=main`. | Must be rejected by E4.5 protected-branch gate. |
| S-WT-4 | U | An agent runs forbidden git ops via the `agend-git` shim. | Deny matrix; #2234 canonical-HEAD deny; recursion guard #1504. |
| S-WT-5 | R | `force_release_worktree` on an out-of-pool path / twice. | Must refuse out-of-pool paths; idempotent on repeat. |
| S-WT-6 | M | Lease → branch merged upstream → auto-cleanup → GC cutover. | Multi-stage soft-mark→hard-delete; protected-ref check at each stage. |

## 5. Task board, teams, decisions (CORE-01..20)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-TB-1 | A | Append to the task event log from two processes at once. | flock must cover the full read-modify-write, not just the append. |
| S-TB-2 | X | Daemon crashes mid-compaction of the 20k-event log. | Compaction must be tmp+rename atomic or events corrupt/lost. |
| S-TB-3 | E | Create a task depending on itself / a cycle. | Circular dep must not infinite-loop (test exists) — verify view-layer too. |
| S-TB-4 | I | `team create` with orchestrator not in members. | Must reject; one-agent-one-team invariant. |
| S-TB-5 | X | `team delete` where one member delete fails midway. | Partial cascade → half-deleted team. |
| S-TB-6 | A | Schema bump: a newer-version event/decision file read by an older binary. | Fail-closed abort — does it brick the whole board, or skip the one record? |
| S-TB-7 | U | `decision` TTL boundary: a 14-day-min decision at exactly the cutover. | Off-by-one expiry; protected-tag bypass. |

## 6. Channels, config, quickstart (CHAN-*, CFG-*, QS-*)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-CH-1 | I | `quickstart` with a malformed bot token / wrong group type (not supergroup). | Format regex + getMe verify + supergroup-migration self-heal. |
| S-CH-2 | X | Quickstart's 3-minute group-detection poll on a slow network. | Hard-coded 3-min, no override → premature timeout. |
| S-CH-3 | R | Re-run `quickstart` (idempotency): existing fleet.yaml + .env token. | Must not overwrite fleet.yaml; only replace token if env explicitly set. |
| S-CH-4 | U | `user_allowlist` absent (legacy open mode). | Fail-closed transition window; an unlisted user messaging the bot. |
| S-CH-5 | E | Set a fleet.yaml env var on the deny-list (GITHUB_TOKEN, LD_PRELOAD). | Must be dropped at spawn; verify completeness + enforcement. |
| S-CH-6 | I | `runtime-config.json` malformed / `display_timezone` invalid IANA. | Silent fallback to defaults → operator unaware config ignored. |
| S-CH-7 | A | Hot-edit fleet.yaml `display_timezone` while daemon runs. | Loaded once at boot → requires restart (not hot-reloadable). |

## 7. CI watch, schedules, recovery (DMN-05..14, 25, 53..55)

| ID | Class | User story | Why it can break |
|----|-------|-----------|------------------|
| S-DM-1 | A | `gh run rerun --failed` on a watched PR (same run_id, new conclusion). | SHA/run_id dedup may suppress the legitimate re-notification. |
| S-DM-2 | X | Daemon down across a DST transition with a pending one-shot schedule. | TZ-adjusted replay (≤24h) may mis/double-fire. |
| S-DM-3 | U | An agent flickers Hung↔Healthy near the silence threshold. | May burn the restart budget → premature Paused. |
| S-DM-4 | A | `restart_daemon`: successor dies between health-probe and flock. | Window where predecessor exits leaving no daemon. |
| S-DM-5 | U | A recycled PID matches an old run dir with unreadable start-token. | PID-only fallback false-matches → wrong daemon targeted. |
| S-DM-6 | X | One event-bus subscriber blocks (slow disk/IO). | Single dispatch thread → whole tick stalls, no timeout/backpressure. |
| S-DM-7 | R | Boot sweep with `AGEND_DAEMON_BOOT_SWEEP_AGE_DAYS` unset/permissive. | Could purge a still-live daemon's run dir. |

---

### How these map to discovery

Phase 3 verification agents were seeded with the highest-risk stories above (S-MCP-2/3/4, S-TUI-1/2,
S-WT-1/2, S-TB-1/2/6, S-DM-4/5/6, and the confirmed CLI doc-drift). Confirmed defects are recorded in
`DEEP-AUDIT-2026-06-29-ISSUES.md`; the feature↔expected↔current matrix is in
`DEEP-AUDIT-2026-06-29-FEATURE-INVENTORY.md`.
