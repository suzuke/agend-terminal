# Task Status Reactions

**Branch:** `feat/task-status-reactions`
**Date:** 2026-04-22

---

## Problem

When a user sends a message via Telegram/Discord, there's no visual feedback on whether the agent received it, is working on it, or has finished.

## Design

### Emoji Lifecycle

```
User sends message
  │
  ▼
👀 (received) — immediate, in handle_message()
  │
  ▼
⏳ (working) — on state transition: Idle/Ready → Thinking/ToolUse
  │
  ▼
✅ (done) — on state transition: Thinking/ToolUse → Idle/Ready
```

### Key Insight: Two Separate Hooks

The three reactions fire from two different code paths:

1. **👀 received** — fires in `handle_message()` (inbound path), where `msg.id` is directly available
2. **⏳ working / ✅ done** — fires in `daemon/supervisor.rs` (state change path), where only the stored `last_inbound_message_id` is available

### Data Flow

```
handle_message(msg)
  │
  ├─ 1. Save msg.id to metadata/{instance}.json as "last_inbound_message_id"
  │     (+ "last_inbound_channel_id" for Discord)
  │
  ├─ 2. React 👀 to msg.id (fire-and-forget, background thread)
  │
  └─ 3. Enqueue to inbox / inject raw (existing flow, unchanged)

supervisor::tick()
  │
  ├─ Detect state change via StateTracker
  │
  ├─ If Idle/Ready → Thinking/ToolUse:
  │     Read last_inbound_message_id from metadata
  │     React ⏳ (fire-and-forget)
  │
  └─ If Thinking/ToolUse → Idle/Ready:
        Read last_inbound_message_id from metadata
        React ✅ (fire-and-forget)
```

### Metadata Storage

Extend `metadata/{instance}.json`:

```json
{
  "display_name": "Alice",
  "last_inbound_message_id": "12345",
  "last_inbound_channel_id": "98765"
}
```

- `last_inbound_message_id` — Telegram: `i32` as string; Discord: snowflake string
- `last_inbound_channel_id` — Discord only (needed for `add_reaction` API); Telegram uses group_id + topic_id from config

### StateTracker Extension

Add two new one-shot flags to `StateTracker` (same pattern as `interactive_prompt_pending_notice`):

```rust
pub struct StateTracker {
    // ... existing fields ...
    working_pending_notice: bool,    // armed on Idle/Ready → Thinking/ToolUse
    done_pending_notice: bool,       // armed on Thinking/ToolUse → Idle/Ready
}
```

In `transition()`:
```rust
// Arm "working" when entering active state from passive
if new is Thinking/ToolUse && prev is Idle/Ready/Starting {
    self.working_pending_notice = true;
}
// Arm "done" when returning to passive from active
if new is Idle/Ready && prev is Thinking/ToolUse {
    self.done_pending_notice = true;
}
```

Consumed by supervisor via `take_working_notice()` / `take_done_notice()`.

### Supervisor Extension

In `tick()`, after existing stall/recovery checks:

```rust
} else if core.state.take_working_notice() {
    Some(NoticeAction::Working)
} else if core.state.take_done_notice() {
    Some(NoticeAction::Done)
}

// After releasing core lock:
NoticeAction::Working => react_to_last_inbound(home, &name, "⏳"),
NoticeAction::Done => react_to_last_inbound(home, &name, "✅"),
```

### Channel-Agnostic React Helper

New function in `ops.rs` (or a shared `channel_react` module):

```rust
/// React to the last inbound message for an instance.
/// Reads message_id from metadata, dispatches to Telegram or Discord.
fn react_to_last_inbound(home: &Path, instance_name: &str, emoji: &str) {
    // 1. Load metadata/{instance}.json → last_inbound_message_id
    // 2. Determine channel type from fleet.yaml
    // 3. Dispatch: telegram::try_telegram_react or discord::try_discord_react
    // 4. Fire-and-forget (spawn thread, log errors, never block)
}
```

## Files to Modify

| File | Change |
|------|--------|
| `src/telegram.rs` | In `handle_message()`: save `msg.id` to metadata as `last_inbound_message_id`, react 👀 (fire-and-forget thread) |
| `src/discord.rs` | In `EventHandler::message()`: save `msg.id` + `channel_id` to metadata, react 👀 |
| `src/state.rs` | Add `working_pending_notice` / `done_pending_notice` flags + `take_*` methods, arm in `transition()` |
| `src/daemon/supervisor.rs` | Add `Working` / `Done` variants to `NoticeAction`, consume new flags in `tick()`, call `react_to_last_inbound` |
| `src/ops.rs` | Add `react_to_last_inbound()` helper that reads metadata and dispatches to the active channel |

## Constraints

- All reactions are fire-and-forget (background thread, errors logged, never block main flow)
- Multiple rapid messages: only the latest `last_inbound_message_id` is tracked — ⏳/✅ react to the most recent message, which is the correct UX
- If no `last_inbound_message_id` exists (agent started without receiving a message), ⏳/✅ are silently skipped
- Telegram rate limits: reactions are lightweight API calls, unlikely to hit limits; serenity handles Discord rate limiting
- State oscillation guard: the one-shot flag pattern (same as `interactive_prompt_pending_notice`) ensures each transition produces at most one reaction
