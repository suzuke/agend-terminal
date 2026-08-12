# Remote notification policy

Telegram and Discord are remote operator surfaces. They should show messages
that either answer an operator request or require an operator decision. Detailed
diagnostics remain available in the TUI, inbox, event log, and application log.

## Delivery inventory

| Source | Trigger | Remote behavior | Rationale |
|---|---|---|---|
| Agent reply (`reply`, progress mirror) | An operator-originated channel turn is active | Preserve verbatim reply, edit, and reaction behavior | This is requested conversation, not a notification |
| Inbound receipt / task-created acknowledgement | The operator sends a message or creates a task from a channel | Reaction/edit or one short acknowledgement | Confirms the operator action landed |
| Fleet activity mirror | Delegate, report, decision, broadcast, or unsubmitted-pane event | Telegram only, and only when `fleet_binding` is configured; one bounded line | Explicit opt-in audit stream; Discord does not implement this sink yet |
| Interactive/permission stall | An active agent is confirmed blocked on a prompt | One alert per blocked episode; only the latest decision surface is included, bounded to 10 lines / 820 characters | The old 40-line pane transcript overwhelmed remote chat and could include an entire shell command |
| Prompt recovery | A previously notified prompt remains blocked long enough, then recovers | One silent short message | Prevents an operator from acting on a stale alert; fast/self-resolving prompts remain log-only |
| Agent lifecycle P0 | Crash, terminal respawn failure, backend exit, auth expiry, confirmed orchestrator hang | One deduplicated Error alert on every configured channel | Immediate operator action may be required |
| Infrastructure P0 | Tick stall, missing canonical repo, stale CI handoff, offline unread obligations | One latched/deduplicated Error alert | Work can otherwise be lost or permanently stalled |
| Recovery exhaustion | Rate-limit retry exhausted, inject repeatedly failed, reclaim cap reached | One terminal alert | Automatic recovery has stopped |
| Context handoff | High context plus missing durable handoff after the nudge | One warning | Manual handoff/restart may be required |
| CI provider warning | CI provider polling/auth/rate-limit failure | One warning subject to provider backoff | This reports an inability to observe CI, not ordinary CI progress |
| PR compliance | A PR first violates a required compliance check | One warning per PR | Merge requires action |
| Reply discharge | An agent explicitly closes a channel reply without answering | One audited notice | The operator owns the right to know that the reply was intentionally omitted |

## Kept off remote channels

The following remain TUI/inbox signals and are not promoted to Telegram or
Discord by the notification layer:

- routine `[AGEND-MSG-PENDING]` pointers and poll reminders;
- ordinary agent state transitions and heartbeat diagnostics;
- successful background cleanup, retry, and scheduler ticks;
- full pane snapshots, shell command bodies, stack traces, and test logs;
- ordinary CI/PR workflow handoffs, which are delivered to the responsible
  agent inbox. Only CI-provider failure uses the remote warning path.

## Routing rules

- `gated_notify` is the common authorization and operator-mode gate for system
  notifications. An absent/empty channel allowlist fails closed. It also applies
  a final 12-line / 1,200-character safety bound, retaining the opening event
  identity and latest action surface when an emitter accidentally supplies a
  transcript-sized body.
- `Sleep` receives Error only; `Away` receives Warn and Error; `Active` receives
  all severities.
- Error-class P0 alerts fan out to every configured channel. Routine Info
  messages never fan out merely because multiple channels exist.
- Explicit agent replies are not shortened by the notification policy.
- Necessary system notifications should lead with the event and affected agent,
  then state whether operator action is required. Raw TUI, command, stack, and
  log content is supporting context, never the notification body.
- A prompt's complete pane text stays local. Remote prompt previews are
  tail-biased so the warning, choices, and cancel hint survive truncation.
