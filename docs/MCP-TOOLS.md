[繁體中文](MCP-TOOLS.zh-TW.md)

# AgEnD MCP Tools Reference (33 tools)

The daemon registry and live `tools/list` schema are authoritative. Role filtering can expose a subset of these 32 registered tools to an instance.

## Action-based Tools

### `task`

Manage task boards. Actions: `create`, `list`, `get`, `claim`, `done`, `update`, `sweep`, `health`, `activity`, `metadata_set`, `metadata_get`, `ack_plan`.

- Core fields include `id`/`task_id`, `title`, `description`, `assignee`, `priority`, `status`, `branch`, `depends_on`, `result`, `due_at`, `project`, and `scope`.
- `list` returns actionable tasks by default; use `include_history:true` to include done/cancelled tasks and filters such as `filter_status` or `filter_assignee` to narrow it.
- `list` is terse by default. Use `verbose:true` for full text or `fields:"minimal"` for only the compact identity/status projection.
- `get` returns one full record by `id` or `task_id`.
- Metadata and plan-ack actions operate on the durable task record; use the live schema for their required keys.

### `decision`

Manage durable decisions and operator questions. Actions: `post`, `list`, `update`, `answer`.

- Decision fields: `id`, `title`, `content`, `tags`, `scope`, `supersedes`, `archive`, `include_archived`, `ttl_days`.
- Questions use `needs_answer`, `options`, `allow_free_text`, `timeout_secs`, and `timeout_default`; `answer` records the selected/free-text response.

### `team`

Manage teams. Actions: `create`, `delete`, `list`, `update`.

- Fields: `name`, `members`, `orchestrator`, `description`, `repository_path`, `project_id`, `accept_from`, `add`, `remove`.
- `project_id` overrides project-board derivation; `accept_from` is the cross-team sender allowlist.

### `schedule`

Manage timed delivery. Actions: `create`, `list`, `update`, `delete`.

- Fields: `id`, `label`, `instance`, `message`, `cron`, `run_at`, `timezone`, `enabled`.
- `list` returns the newest three history entries and `runs_total` by default; use `full_history:true` for all retained entries, up to 50.
- `fire_strategy` is `always` or `until_success`; the latter requires `linked_task_id`.

### `deployment`

Manage batch deployments. Actions: `deploy`, `teardown`, `list`.

- Fields: `name`, `template`, `branch`, `directory`.

### `ci`

Manage CI watches. Actions: `watch`, `unwatch`, `status`.

- Fields: `repository`, `branch`, `interval_secs`, `next_after_ci`, `review_class`, `ci_provider`, `ci_provider_url`, `task_id`, `head_sha`.
- Use `repository` (GitHub `owner/repo`), not `repo`. `watch` may derive it from the caller's binding; `unwatch` requires it explicitly.
- Generic `main`/`master` watches are rejected. A protected-ref exact-head watch requires a full 40/64-hex `head_sha`, `task_id`, explicit `next_after_ci`, GitHub, and an authorized orchestrator/operator caller.

### `repo`

Manage repository worktrees, branch cleanup, and PR merge. Actions: `checkout`, `release`, `cleanup_init_commits`, `cleanup_merged_branches`, `merge`.

- Common fields include `repository_path`, `repository`, `branch`, `path`, `instance`, `bind`, `task_id`, `expected_head`, and `checkout_purpose`.
- `checkout bind:true` provisions and binds; `bind:false` creates an inspection worktree.
- `checkout_purpose:"disposable_review"` creates typed review provenance. It requires `bind:true`, a non-empty `task_id`, a full `expected_head`, and a branch proven new locally and on `origin`.
- `cleanup_merged_branches` is dry-run by default and requires `confirm_ids` plus `audit_reason` when applying.
- `merge` uses `pr`; `force:true` requires `force_reason` and is audited.

### `health`

Manage blocked health state. Actions: `report`, `clear`.

- `report` uses the caller identity and accepts `reason` (`rate_limit`, `quota_exceeded`, or `awaiting_operator`), optional `retry_after_secs`, and `note`.
- `clear` requires target `instance`; an optional `reason` limits which blocked reason is cleared.

## Communication

### `send`

Send to one instance or broadcast. This is the unified inter-agent messaging tool.

- Required: `message`. Route with one of `instance`, `instances`, `team`, or `tags`.
- `request_kind`: `query`, `task`, `report`, or `update`; typed reports should set `report_purpose`.
- Task fields include `task_id`, `success_criteria`, `context`, `branch`, `bind`, `worktree_binding_required`, `eta_minutes`, `reporting_cadence`, `expect_reply_within_secs`, and `next_after_ci`.
- Broadcast task dispatches require an existing `task_id`. The current single-target compatibility path can auto-create when it is omitted, but explicit `task action=create` plus `task_id` is the stable contract.
- Thread/correlation fields: `correlation_id`, `parent_id`, `thread_id`.
- Busy/review fields include `force`, `force_reason`, `second_reviewer`, `second_reviewer_reason`, `review_class`, plan-ack fields, typed review-assignment fields, `reviewed_head`, and `artifacts`.
- Report controls include `terminal`, `ack_inbox`, and `triaged`; fire-and-forget tasks can use `no_report_expected`.

### `inbox`

Drain or manage the calling instance's durable inbox.

- No arguments drains unread messages and marks them `delivering`; it does not yet mark them processed. Returned rows include durable `delivery_count` and `first_delivered_at`; redeliveries also appear in the production response's `redelivery_history` array with their message id and first-delivery time, without changing canonical message identity/text.
- `message_id` describes one message; `thread_id` fetches a thread. Optional `instance` scopes authorized lookups.
- `action:"ack"` confirms one current delivering `message_id`, or the whole in-flight batch when the ID is omitted. A targeted ID may acknowledge a reclaimed/requeued row only when durable delivery history proves prior delivery; the response reports `outcome:"acked-after-reclaim"`. Omitted-ID ack remains conservative and never settles a never-delivered unread row. Storage failures report `outcome:"error"` with `code:"inbox_ack_failed"` rather than being returned as `no-delivering-rows`.
- `action:"clear"` compact-clears non-obligations while keeping unanswered queries/tasks unread and reporting them in `requires_response`.
- `action:"discharge"` requires `message_id` and non-empty `reason`; it closes a channel-reply obligation without answering and notifies the operator.
- Re-draining implicitly acknowledges the previous delivery batch; an unconfirmed batch can be reclaimed for redelivery after about ten minutes. A fresh session reset requeues unconfirmed rows for successor recovery instead of blanket-processing them. This intentionally replaces #159's old settle rationale: stamping `read_at` hid stale delivery but could silently lose an unconfirmed message.

### `reply`

Reply to the user/operator through an external channel; do not use it for inter-agent messages.

- Required: `message`.
- `message_id` routes by the original inbox message's channel and settles that row after a successful send.
- Optional `task_id` and `correlation_id` preserve reply-to correlation.
- Pair `default_action` with `timeout_secs` to record a timed default decision.

### `operator_page`

Page the **operator** on their Telegram, independent of any inbound channel binding — for milestones they explicitly asked to be told about while away or asleep. This is the agend-side answer to a gap in the harness: `PushNotification`'s mobile leg only reaches a phone when Remote Control is connected, and `reply` needs an inbound message to answer, which operator input typed in the TUI never creates.

- Required: `message`. Plain text, truncated at 1000 characters, always prefixed with the calling instance's name. Before the cap and the prefix the body is normalised to ONE line: every **Cc** control character (LF, CR, TAB, VT, FF, NEL, the rest of C0/C1), every Unicode **White_Space** character (NBSP `U+00A0`, `U+1680`, `U+2000`–`U+200A`, `U+202F`, `U+205F`, `U+3000`, and the mandatory breaks `U+2028`/`U+2029`), every general-category **Cf** format character (ZWSP `U+200B`, ZWNJ/ZWJ, the bidi LRM/RLM/LRE/RLE/PDF/LRO/RLO set, the `U+2066`–`U+2069` isolates, `U+FEFF`) and every character carrying the Unicode **`Default_Ignorable_Code_Point`** property (CGJ `U+034F`, the variation selectors `U+FE00`–`U+FE0F` and `U+180B`–`U+180F`, the Hangul fillers) becomes ONE space, and runs of spaces collapse. Category **Mn** as a whole is deliberately NOT stripped — combining marks are how Vietnamese, Hebrew and Devanagari are written — so only the default-ignorable part of it is taken; the accepted cost is that `U+FE0F` is stripped from an emoji, which may then render in its text presentation.

  If what is left still contains the daemon's sender marker `[operator-page from ` — matched **case-insensitively**, so `[Operator-Page From ops]` counts — the page is **REFUSED** with `marker_in_body`. The refusal runs immediately after the enabled switch and ahead of every other gate: before authority, before deliverability and before any budget claim, so a forged body can never cost the caller a rate slot, and the attempt is logged at `warn!` with the calling name so the operator can see it. It sits behind the switch on purpose — while paging is OFF the tool is inert and answers `operator_page_disabled`, so a disabled feature is not a way to make the daemon write log lines. An earlier version instead rewrote a literal marker to `[quoted: operator-page from ` and delivered the page; that silently MUTATED operator-visible text (no flag in the payload, nothing logged) and was defeated by case variants and by NBSP/ZWSP/RLO spellings anyway, so it is withdrawn. A legitimate page essentially never contains the marker, so refusing costs nothing and makes an attempt detectable.

  Exactly what is and is not covered, stated precisely because an earlier version of this paragraph claimed more than the code delivers:

  - **COVERED.** Every mandatory line break and every character that cannot survive verbatim: control characters (**Cc**), Unicode **`White_Space`** including NBSP, format characters (**Cf**) such as ZWSP and the bidi overrides, and the **`Default_Ignorable_Code_Point`** set (CGJ `U+034F`, the variation selectors). So the marker cannot begin a line, and it cannot be spelled with an invisible-format character or a look-alike space.
  - **NOT COVERED.** A marker spelled with **homoglyphs** — `[оperator-page from ops]`, with Cyrillic `о` `U+043E` for Latin `o` — is **not detected and IS delivered**. Every character in it renders, so no test for invisibility can see it, and confusable folding is not attempted here.
  - **Why the residual is bounded** — as the mitigation it is, and not more: the body is flattened to ONE line and the daemon's own prefix is always first, so a homoglyph forgery can only ever appear **mid-line, after a genuine `[operator-page from <caller>]` prefix**. It cannot open a line and it cannot displace the real sender.

  The honest limit stands: a client that soft-wraps a long page can still start a visual row mid-body, and a body is still free to contain other sender-looking prose. None of this makes a page unimpersonatable.
- **Orchestrator-only, bound to a LIVE instance.** The `instance` a call carries is resolved against the daemon's live registry before anything else: a name matching no running instance — or matching two — is refused with `unknown_caller`; a caller listed by more than one team is refused with `ambiguous_team` rather than answered from map order; a caller that is not its team's current orchestrator is refused with `not_orchestrator` and told who to route through. A standalone bridge call has no registry to resolve against and is refused with `no_live_identity`.

  The honest limit: every agent and the daemon share ONE OS user, so a seat that presents the orchestrator's live name **is** admitted. The gate rejects names that mean nothing; it cannot reject a seat that lies. What bounds the damage is the rest of this list — default-off, the operator-only switch, three pages an hour, one dedicated topic inside the allowlisted group, and fail-closed budget state.
- **Off by default, and the switch is operator-only.** It lives in the daemon's runtime config, which the `config` MCP tool can READ but not write (`set` moved to the CLI in #2548). The operator turns paging on with:

  ```
  agend-terminal admin config-set operator_page.enabled true
  ```

  and picks the destination topic with `agend-terminal admin config-set operator_page.topic_name <NAME>` (default `operator-notifications`). A `channel.operator_page` stanza in `fleet.yaml` is no longer read at all: fleet.yaml is agent-writable, so a master switch there is one its own subjects could flip. Telegram itself still needs `channel.user_allowlist` in `fleet.yaml` — outbound is fail-closed without it.
- **Telegram specifically.** The page has to be able to reach the operator's phone: an authorized Discord channel does not count, and a call that cannot reach telegram is refused with `not_delivered` without spending a rate slot.
- **Rate-capped at 3 per orchestrator per rolling hour.** The excess is DROPPED, never queued; the refusal carries `retry_after_secs` so the caller can fall back to recording the milestone in `SESSION-HANDOFF.md`. The counter is authoritative in the daemon's memory behind a lock and is snapshotted to `$AGEND_HOME/operator_page_rate.json` so a restart does not refill the budget.

  **The snapshot is seeded by the operator, never by the daemon.** `agend-terminal admin config-set operator_page.enabled true` lays down an empty snapshot if there is not one already (it never clobbers an existing one, so re-running it does not refund spent pages). The daemon refuses to invent one: an ABSENT snapshot at startup is treated exactly like a corrupt one and DENIES. That closes a bypass — deleting the file and forcing a daemon restart used to re-initialise the budget empty, turning 3-per-hour into 3-per-restart.

  Every untrustworthy state refuses with `budget_unavailable` — a code distinct from `rate_limited` — plus a `cause` naming which condition tripped it: `snapshot_absent`, `snapshot_corrupt`, `snapshot_missing` (it vanished while the daemon was running), `snapshot_unusable`, `snapshot_unwritable`. **The remedy in every case is the operator re-running the enable command** to re-seed; a corrupt snapshot must be repaired or deleted first.

  **What that remedy costs, before you run it.** For `snapshot_absent` and `snapshot_corrupt` the daemon no longer holds the spent count for that home — it was destroyed with the snapshot — so re-seeding writes an empty counter and **STARTS A NEW ROLLING HOUR**: pages already spent inside the current hour are forgotten and the caller gets a full budget again. The refusal hint says so before the operator acts, and the seeding logs a `warn!` naming the snapshot path. For `snapshot_missing` and `snapshot_unwritable` memory still holds the true count, so re-seeding writes it back and the hour is preserved; those hints carry no such warning. This is not a claim that the snapshot is tamper-proof — see the next paragraph. It is tamper-EVIDENT, and the reset is recoverable only by a deliberate human act.

  Say what this is and is not: every agent and the daemon share ONE OS user, so this file is **not tamper-proof** and an agent can delete it whenever it likes. What the design buys is that tampering can only ever DENY, that it is logged with the snapshot path, and that recovery runs through an operator-only command. An agent can silence its own pager; it cannot refill the hour BY ITSELF — the only route back to a full budget runs through the operator command above, which now states, before it is run, that it restarts the rolling hour.
- **Routing.** Pages land in a dedicated forum topic (default `operator-notifications`), auto-created and registered on first use, so they collect in one place the operator can mute. If that topic cannot be created the page falls back to the sender's own topic — both live inside the allowlisted group.
- **Operator Away/Sleep mode does NOT suppress pages.** That is deliberate: the feature exists because the operator was asleep and asked to be woken for milestones. The controls for pages are the `enabled` switch (the master off) and the hourly cap — not the mode. Ordinary daemon notices remain mode-gated as before.

### `download_attachment`

Download a Telegram multimedia attachment and return its local path.

- Required: `file_id`.

## Instance Lifecycle

### `create_instance`

Create one instance or a homogeneous/heterogeneous team.

- Fields include `name`, `backend`, `model`, `model_tier`, `args`, `working_directory`, `branch`, `task`, `role`, `env`, `topic_binding`, `team`, `count`, `backends`, `layout`, and `target_pane`.

### `delete_instance`

Stop and remove an instance.

- Required: `instance`. A creator-path delete of an instance with in-flight work additionally requires `force:true` and non-empty `force_reason`; the override is audited.

### `start_instance`

Start a stopped instance.

- Required: `instance`.

### `restart_instance`

Restart an instance.

- Required: `instance`; optional `mode` (`resume` or `fresh`), `reason`, and `force`.
- `resume` is the default and preserves backend conversation state.
- `fresh` starts clean and refuses a dirty bound worktree unless `force:true` is explicitly supplied.

### `set_model`

Persist exactly one model intent (`model` or `tier`) for an instance; setting one clears the other. `restart:true` applies it immediately, otherwise it takes effect on the next respawn.

- Required: `instance` and exactly one of `model`/`tier`.

### `bind_topic`

Create a deferred/eligible Telegram topic binding.

- Required: `instance`; optional `channel` currently defaults to `telegram`.
- Already-bound instances are an idempotent no-op; `skip` mode is not eligible.

### `list_instances`

List active instances, or pass `instance` for detail. Output is compact by default; `verbose:true` or `include_evidence:true` includes observed-status evidence. The response also exposes operator mode.

### `set_metadata`

Set display metadata for the caller. Actions: `display_name`, `description`.

- `display_name` uses `name`; `description` uses `description`.

### `set_waiting_on`

Declare the caller's current wait condition; send an empty `condition` to clear it.

### `interrupt`

Send ESC to a target PTY.

- Required: `instance`; optional `reason` and `snapshot`. Set `snapshot:true` to return a post-ESC diagnostic snapshot.

### `move_pane`

Move an instance pane to a TUI tab.

- Required: `instance`, `target_tab`; optional `split_dir` (`horizontal` or `vertical`).

### `pane_snapshot`

Read ANSI-stripped PTY scrollback.

- Required: `instance`; optional `lines`, `head`, and `to_file`.
- `to_file:true` stores the full capture under `$AGEND_HOME/captures/` and returns a compact response.

### `instance`

Read-only folded alias. Actions: `list`, `pane_snapshot`; semantics match the standalone tools above.

## Worktree & Binding

### `bind_self`

Recover or rebind the calling instance to a branch worktree. Prefer `repo action=checkout bind:true` for fresh work.

- Required: `branch`; optional `repository_path`, `rebase_mode`, and `task_id`.
- Rejects protected branches and cross-agent lease conflicts. It does not silently create a CI continuation.

### `release_worktree`

Guardedly release the exact daemon-managed worktree and binding. The normal path preserves WIP and checks a fresh binding fingerprint; it is idempotent after success.

- Required: `instance`; optional `dry_run` and `force`.
- `force:true` additionally requires `branch`; `repository_path` is an optional cleanup hint. Markerless, opaque, ambiguous, or mismatched state is preserved.

### `binding_state`

Non-destructively report binding content, worktree/marker state, signature diagnostics, CI subscriptions, in-flight guard, and branch holders.

- Required: `instance`.

### `revoke_review_assignment`

Revoke a reviewer assignment by exact CAS identity. Authorized for the owning team orchestrator or operator; repeated revoke is idempotent.

- Required: `assignment_id`.

### `usage_limit_takeover`

Operator-only PREPARE step for a persisted usage-limit takeover episode. It writes the durable prepared journal but does not execute the takeover.

- Required: source `instance` and exact `episode_id`.

## Daemon Operations

### `config`

Read runtime configuration. Actions: `get`, `list`; MCP mutation is not supported.

- `get` requires `key`.
- Current keys: `dev_idle_threshold_secs`, `fleet_idle_threshold_secs`, `fleet_idle_ack_ttl_secs`, `hang_auto_recovery_enabled`, `usage_limit_propagation_enabled`, `idle_watchdog_enabled`, `show_pane_state`, `copy_on_select`, `dim_unfocused_panes`, `observed_badge`, `context_alert_pct`, `context_handoff_pct`, `context_handoff_escalate_pct`, `experimental.tool_cli_enabled`, `operator_page.enabled`, `operator_page.topic_name`.
- Change a value with `agend-terminal admin config-set <KEY> <VALUE>`.

### `restart_daemon`

Request a graceful daemon restart. Parameters: none.

- Default standalone mode self-respawns a successor, waits for its health gate, then exits normally; no external supervisor is required.
- With `AGEND_RESTART_HANDOFF=0`, legacy mode exits with code 42 and requires an installed service supervisor or wrapper; it returns failure if none is detected.
- In Unix `agend-terminal app` mode, restart preflights and re-execs in place with the same PID. A successful preparation response is followed by the connection dropping during re-exec.
- Windows app mode remains fail-closed; quit and relaunch instead.
- A shared gate permits at most one restart in flight; a concurrent request is retryable.

## Bridge and daemon-proxy contract

The daemon is the only authority for the tool registry, authorization, task
state, and side effects. `agend-mcp-bridge` is a near-zero-state relay; it has
no local tool implementation or filesystem fallback.

The experimental `agend-terminal tool <NAME>` command uses the same daemon
handlers, names, arguments, and instance claim as MCP. An instance should use
one invocation surface at a time; translate mechanically when coordinating
with a peer using the other surface.

```text
MCP client
  │ stdin/stdout: newline-delimited JSON-RPC
  ▼
agend-mcp-bridge
  │ authenticated loopback TCP: newline-delimited JSON
  ▼
AgEnD daemon (`/mcp` dispatcher)
```

### Framing and authentication

Both stdio and TCP carry one JSON object per line. `Content-Length` framing is
not supported. The bridge handles `initialize`, `ping`, and JSON-RPC
notifications locally; it proxies `tools/list` and `tools/call` after discovering
the active run directory, opening a persistent loopback connection, and
authenticating with the daemon cookie plus its bridge PID.

| Boundary | Timeout | Purpose |
|---|---:|---|
| Daemon, before authentication | 5 seconds | Bound idle or partial authentication attempts |
| Bridge, waiting for a daemon response | 120 seconds | Bound a stalled proxy request |
| Daemon, after authentication | No session read timeout | Permit long-lived idle MCP sessions |
| Daemon tool execution | 5 / 30 / 60 seconds | Fast, default, and slow execution bands |

The daemon checks the authenticated bridge PID approximately every two seconds
and closes the session after PID death or TCP EOF.

### Request identity, retry, and execution timeout

Every proxied request gets a UUIDv4 `request_id`. A retryable transport failure
causes at most one reconnect/retry with the same ID; daemon deduplication keeps
the side effect exactly-once. Startup discovery retries every 100 ms for up to
30 seconds. Application errors are returned immediately and are never treated
as transport failures.

Read-only or idempotent operations that exceed their 5/30/60-second band return
a retryable timeout. A side-effecting operation continues in the background and
returns `accepted_in_progress`; callers must observe the task, inbox, or status
surface and must not resend it. The bridge's 120-second timeout is only a
transport backstop.

The bridge retains only its connection and one successful identical
`tools/call` result for 500 ms to absorb an immediate duplicate. Failed calls
never seed that cache.

### Fail-closed behavior and source ownership

- daemon unavailable at startup: retry for 30 seconds, then return a visible
  JSON-RPC error;
- connection loss during a request: reconnect and retry once with the same ID;
- retry failure or daemon application error: return the visible error;
- bridge exit: daemon closes the authenticated session;
- no daemon: no local or filesystem execution path exists.

The implementation owners are `src/bin/agend-mcp-bridge.rs` (framing,
connection, identity, retry), `src/api/mod.rs` (authentication and peer-PID
monitoring), `src/api/handlers/mcp_proxy.rs` (dispatch and timeout bands), and
`src/mcp/registry.rs` (authoritative registry and execution classes).
