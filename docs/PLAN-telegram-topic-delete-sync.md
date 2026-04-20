# PLAN: Telegram topic delete → App pane sync

**Branch:** `feat/telegram-topic-delete-sync` (to be created when implementation starts)
**Date:** 2026-04-20

---

## 1. Background

In app mode, deleting a pane or tab propagates to Telegram (the corresponding
forum topic gets closed + deleted via `maybe_delete_telegram_topic` at
`src/app/telegram_hooks.rs:73`). The reverse direction works only partially:

- **Close topic** (Telegram UI → "Close topic"): `forum_topic_closed` service
  message fires → `src/telegram.rs:216` handler calls `api::call(DELETE)` →
  `api.rs:350` emits `TuiEvent::InstanceDeleted` → `tui_events.rs:157`
  `handle_instance_deleted` removes the pane. **Works correctly.**
- **Delete topic** (Telegram UI → "Delete topic"): bot receives no update at
  all, so no handler runs, pane stays attached to a dead instance.

## 2. Root cause

Telegram Bot API (through the current and all historical versions) does **not**
emit a `forum_topic_deleted` service message. Confirmed against:

- `teloxide-core` schema — only `forum_topic_created / edited / closed /
  reopened / general_hidden / general_unhidden` exist.
- Bot API changelog from 6.3 onwards — no deletion event introduced.
- Upstream issues `tdlib/telegram-bot-api#286`, `rubenlagus/TelegramBots#1084`,
  python-telegram-bot discussion #4223 — ecosystem-wide confirmed limitation.
- AgEnD (TypeScript sibling project) also only listens to `forum_topic_closed`
  (`src/channel/adapters/telegram.ts:193`) — same limitation, same workaround.

`getForumTopic` / `getForumTopicInfo` do not exist in Bot API; only MTProto
(requires a user session, not a bot) has `channels.getForumTopics`. Switching
to MTProto is an architectural rewrite and out of scope.

## 3. Rejected alternatives

| Approach | Why rejected |
|---|---|
| Periodic probe (send/edit to every bound topic every N min) | Burns group rate limit (20 msgs/min/group, 30/sec global); all other Telegram traffic suffers a flood-wait once quota hits |
| `editForumTopic` no-op as cheapest active probe | Same rate limit bucket — nice-to-have only, not architecturally meaningful |
| Ask users to close-then-delete only | Close already works; delete UX is user preference, can't force |
| Switch to MTProto (TDLib/grammers) | Requires user session, full auth/deploy change, massive scope creep for one edge case |

## 4. Chosen approach: error-driven cleanup

On the next outbound Telegram call that targets a deleted topic, Telegram
returns `HTTP 400 Bad Request: message thread not found`. Treat this as the
authoritative deletion signal and route through the same cleanup path as
`forum_topic_closed`.

**Why this is acceptable:**

- Zero extra request cost (no polling, no probes).
- Deletion is rare; the "next send" delay is typically seconds to minutes,
  acceptable for the use case.
- Ecosystem consensus — python-telegram-bot, telegraf, grammy, AgEnD all land
  on this pattern.
- Defence-in-depth: still works as a safety net even if Telegram later adds a
  proper deletion event (we'd prefer the event but the fallback remains sound).

## 5. Design

### 5.1 Shared cleanup helper

Extract the existing logic at `src/telegram.rs:220-234` into a helper:

```rust
fn cleanup_deleted_topic(state: &mut TelegramState, instance_name: &str, tid: i32, home: &Path)
```

Responsibilities:
1. Remove `tid` from `state.topic_to_instance`.
2. Remove `instance_name` from `state.instance_to_topic`.
3. Call `api::call(DELETE, { name: instance_name })` → triggers
   `TuiEvent::InstanceDeleted` in owned-app mode.
4. `fleet::remove_instance_from_yaml(home, instance_name)`.
5. Structured tracing log for observability.

Both the `forum_topic_closed` handler and the new send-error path call this
helper — single source of truth, can't drift.

### 5.2 Error classification at send sites

Target call sites:
- `send_with_topic` (`src/telegram.rs:~680`) — main send path for agent replies
  and vterm tails.
- Any other place that issues a per-topic API call and holds
  `(instance_name, topic_id)` context.

Classification rule (both conditions required, to guard against Telegram
changing the description wording):

```rust
err.error_code() == Some(400)
  && err.description().to_lowercase().contains("message thread not found")
```

On match: invoke `cleanup_deleted_topic` with the owning `instance_name` +
`tid`. On mismatch: log + propagate the original error unchanged.

### 5.3 Documentation

- Add a paragraph in `README.md` (Telegram section) explaining the Close vs
  Delete behaviour so users know Close is the clean path.
- Note in `docs/PLAN-terminal-app.md` that topic-delete cleanup is lazy (fires
  on next agent send) — link to this plan.

## 6. Out of scope (explicitly)

These are noted for future reference but deliberately not part of this plan:

- **Self-close vs user-close distinction.** AgEnD's `TopicArchiver`
  (`src/topic-archiver.ts`) auto-closes idle topics and uses
  `archived-topics.json` to filter its own close events. We have no
  auto-archiver yet; if we add one, revisit this plan to add the same filter.
- **Channel abstraction refactor.** If a Discord backend lands, normalise
  `forum_topic_closed` (Telegram) and `channelDelete` (Discord Gateway) into a
  single `BindingRevoked { instance }` event at the `Channel` trait layer.
  That's a separate plan — AgEnD's `src/channel/` layout is a reasonable
  reference when that time comes.
- **Active probing via `editForumTopic` no-op.** Rate-limit characteristics
  make this strictly worse than error-driven; only revisit if we get a
  concrete product ask for sub-second deletion detection.

## 7. Verification

### 7.1 Unit / integration test

Add a test that asserts the classification rule + cleanup wiring:

- Craft a mock Telegram send error with `error_code: 400` and
  `description: "Bad Request: message thread not found"`.
- Invoke the classifier + helper.
- Assert: `topic_to_instance` / `instance_to_topic` entries gone, fleet.yaml
  entry removed, `TuiEvent::InstanceDeleted` sent to the TUI channel.

Place in `tests/integration.rs` or a new `tests/telegram_topic_cleanup.rs`,
consistent with the repo's existing integration test layout.

### 7.2 Manual acceptance

1. Owned app mode, Telegram connected, spawn an agent → confirm topic created.
2. In Telegram client, **Delete** (not Close) the topic.
3. Send any command that causes the agent to emit to Telegram (e.g. inject a
   prompt via another channel, or wait for a scheduled message).
4. Expected: within seconds of the next send attempt, the agent's pane
   disappears from the TUI and `fleet.yaml` entry is removed.

### 7.3 Regression

Existing `forum_topic_closed` handler must keep working — the extracted
`cleanup_deleted_topic` helper is invoked from both call sites, so the Close
flow should be byte-for-byte equivalent to today's behaviour. Add an assertion
(or retain the existing test) that Close still removes the pane.

## 8. Implementation checklist

- [ ] Create branch `feat/telegram-topic-delete-sync` in a worktree.
- [ ] Extract `cleanup_deleted_topic` helper in `src/telegram.rs`, migrate the
      existing `forum_topic_closed` handler to call it.
- [ ] Add error classification + cleanup invocation at `send_with_topic`.
- [ ] Sweep other per-topic send sites (grep for `send_with_topic`
      / `send_reply` / any direct `bot.send_message` with `message_thread_id`)
      and wire the same classification.
- [ ] Unit test for classifier + cleanup wiring.
- [ ] Update `README.md` (Telegram section).
- [ ] Update `docs/PLAN-terminal-app.md` with a one-line pointer to this plan.
- [ ] Manual acceptance per §7.2.
- [ ] Local merge to main (no PR — per "Local merge without PR" project
      convention for pure local / no-CI changes).

## 9. Success criteria

- Deleting a Telegram topic causes the corresponding pane to close within one
  normal agent send cycle.
- No additional background polling or probe traffic introduced.
- Close-topic behaviour unchanged.
- No new failure modes on Telegram API errors unrelated to topic deletion
  (they must still propagate as today).
