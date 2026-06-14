[繁體中文](FEATURE-channels.zh-TW.md)

# Channels — Telegram / Discord Integration

The Channels system lets operators talk to agents from Telegram or Discord without opening a terminal. Each agent maps to its own Telegram forum topic, and messages are mirrored bidirectionally.

## Usage Scenarios

> **Target audience:** Both operators and agents.

An operator is away from the keyboard and wants to check whether an agent is still alive, send a quick instruction, or read the latest response without opening a terminal. Telegram becomes the control surface, and the daemon keeps the topic state in sync.

An agent posts a reply after finishing a task. The reply is mirrored back into the same topic, so the operator can follow the conversation in place instead of chasing status across different tools.

When the fleet grows, the one-topic-per-agent model keeps conversations isolated. That makes it practical to monitor several agents at once without mixing their context together.

## Design Goals

When a team is working with multiple agents, the operator needs a communication path that is always available. Channels provides:

- **Bidirectional messaging**: post in a Telegram topic and the agent receives it; the agent replies and the operator sees it in the same topic.
- **One topic per agent**: conversations stay isolated instead of collapsing into one shared stream.
- **Platform-agnostic core**: the main code does not depend on Telegram specifics; it talks through a Channel trait.
- **Automatic setup**: the daemon creates missing topics at startup, so there is no manual bootstrap step.

---

## Quick Start

### 1. Configure Telegram Bot

Add the channel config to `fleet.yaml`:

```yaml
channel:
  telegram:
    bot_token: "123456:ABC-DEF..."
    group_id: -1001234567890
    user_allowlist:
      - 12345678    # your Telegram user ID
```

- `bot_token`: obtained from @BotFather
- `group_id`: Telegram supergroup ID (must have Topics enabled)
- `user_allowlist`: Telegram user IDs allowed to interact with agents

### 2. Start the daemon

```bash
agend-terminal start
```

On startup the daemon will:

1. Load the channel settings from `fleet.yaml`.
2. Load `topics.json` (the topic ID ↔ agent mapping).
3. Create missing Telegram forum topics for agents that do not have one yet.
4. Create the fleet binding topic for cross-agent and team-wide notifications.
5. Start polling for incoming messages.

### 3. Start chatting

Open the agent's topic in the Telegram group and just type. The agent will answer in the same topic.

---

## Core Concepts

### Channel Trait

All channel implementations share one trait interface:

| Method | Meaning |
|---|---|
| `send` | Send a message to a binding |
| `edit` | Edit a previously sent message |
| `delete` | Delete a message |
| `create_binding` | Create a channel binding for an agent |
| `remove_binding` | Remove a binding |
| `create_topic` | Create a new forum topic |
| `poll_event` | Poll for incoming events |

The core code only talks to the trait. It never calls Telegram APIs directly. If we add Discord or Slack later, it only needs to implement this trait.

### Binding

A binding links an agent to a channel. Each binding carries platform-specific addressing information (such as a Telegram topic ID), but the core code does not need to know those details — the binding is opaque.

```
agent "dev" ← binding → Telegram topic #42
agent "reviewer" ← binding → Telegram topic #43
```

### Capabilities

Each channel declares the features it supports, and the core code uses that to decide its degradation behaviour:

| Capability | Telegram | Discord |
|---|---|---|
| Native threads | Yes (forum topics) | Yes (threads) |
| Markdown | MarkdownV2 | Discord Markdown |
| Attachment upload | Yes | Yes |
| Message editing | Yes | Yes |
| Emoji reactions | Yes | Yes |
| Typing indicator | Yes | Yes |
| Message length limit | 4096 bytes | 2000 chars |
| Delete events | No | Yes |

Unsupported features degrade silently instead of raising an error.

### Topics and Registry

The daemon keeps a registry of topics so it can reconcile what is live in Telegram with what is known on disk. That registry is the source of truth for topic routing, while the topic itself is the operator-visible conversation surface.

### Inbound vs Outbound

Channels handles two directions:

- **Inbound**: operator or external events flow into the daemon, then into the agent.
- **Outbound**: the agent's reply is mirrored back to the same channel surface.

The code path is symmetric enough that debugging either side usually starts from the same topic entry and binding record.

---

## Topics Mapping

### topics.json

The mapping between agents and Telegram topics is persisted in `topics.json`:

```json
{
  "42": "dev",
  "43": "reviewer",
  "100": "general",
  "500": "__fleet__"
}
```

- The key is the Telegram forum topic ID (a stringified number)
- The value is the agent's instance name
- `__fleet__` is a reserved sentinel, used for cross-agent team-wide notifications

### Automatic Topic Creation

On startup, for every agent defined in `fleet.yaml` but without a corresponding topic in `topics.json`, the daemon automatically creates a forum topic.

When you add an agent in the TUI via `Ctrl+B c`, a topic is also created and registered automatically.

### Orphan Topic Cleanup

Use the `doctor topics` command to inspect and clean up orphan topics:

```bash
# Inspect topic status
agend-terminal doctor topics

# Clean up orphan topics
agend-terminal doctor topics --cleanup
```

Topics fall into two categories:
- **Live**: present in both `topics.json` and `fleet.yaml`
- **Orphan**: present in `topics.json` but not in `fleet.yaml` (the agent was deleted but the topic was not cleaned up)

---

## Message Flow

### Inbound (Telegram → Agent)

```
1. A user sends a message in a Telegram forum topic
2. The polling thread detects the new message and obtains the topic_id
3. The target agent is resolved through the topic_to_instance mapping
4. The message is written into the agent's inbox
5. The agent calls the inbox tool to read the full message
```

### Outbound (Agent → Telegram)

```
1. The agent calls the reply MCP tool
2. Dedup check: identical content is not re-sent within 5 seconds
3. instance_to_topic is queried to obtain the target topic_id
4. The message is sent through the Telegram Bot API
5. The message_id is recorded for later editing/deletion
```

### Mirror Skip

An agent's reply appears both in the Telegram topic and in the PTY terminal output. To prevent the PTY mirror from forwarding the agent's own reply a second time, the system sets the `mirror_skip_until_next_turn` flag before sending. This flag is reset automatically on the next round of user input.

---

## Deduplication

Prevents duplicate messages caused by the following race conditions:

- The app and the daemon poll CI watch at the same time, each sending one notification
- The PTY mirror and the reply tool send identical content at the same time
- Send paths that retry logic does not fully guard

Deduplication uses a content hash + TTL window (default 5 seconds):

```yaml
# fleet.yaml can adjust the TTL
channel:
  dedup_ttl_secs: 5
```

An identical (instance, topic, content hash) is sent only once within the TTL. The in-memory cap is 1024 entries, using an insertion-order LRU policy.

---

## Notification Gate

An agent's outbound notifications (CI status, task completion, etc.) are disabled by default (fail-closed). You must set `user_allowlist` in `fleet.yaml` to enable them:

```yaml
channel:
  telegram:
    user_allowlist:
      - 12345678
```

When it is not set, all outbound notifications are silently dropped without raising an error. This prevents an unconfigured bot from accidentally leaking information to an unauthorized group.

---

## Fleet Binding Topic

The fleet binding is a special topic used to display cross-agent team events:

| Event type | Format |
|---|---|
| Task delegation | `[lead → dev] DELEGATE 修復 #1177 (#t-...)` |
| Result report | `[dev → lead] REPORT PR 已建立 (#t-...)` |
| Decision publication | `[lead] DECISION 使用 prefix match (#d-...)` |
| Broadcast | `[lead → 3 agents] BROADCAST merge freeze` |

The operator can see an overview of the whole team's activity in a single topic, instead of checking each agent's topic one by one.

---

## Self-Healing

### Topic Deleted

If a Telegram topic is deleted unexpectedly (an admin action or an API error), the system automatically:

1. Detects the topic-deleted error
2. Clears the invalid topic mapping
3. Recreates the topic
4. Retries the send

### Supergroup Migration

When a Telegram group is upgraded to a supergroup, the `group_id` changes. After the system detects a `MigrateToChatId` error, it:

1. Reads the new chat ID
2. Updates `group_id` in `fleet.yaml`
3. Retries the send

Neither case requires operator intervention.

---

## Multi-Channel Support

Telegram is currently supported, with Discord as a reserved interface (feature gate). The architecture is designed to support using multiple channels at once:

- Each channel registers independently in the global registry
- Each agent can bind to a different channel
- Inbound events are merged uniformly by the dispatcher
- Outbound messages are routed to the correct adapter based on the binding's channel kind

---

## Telegram Behaviour

### Topic creation

If an agent has no topic yet, the daemon will create one automatically when the channel is enabled. This keeps fleet bootstrap simple: configure the bot, start the daemon, and let the registry populate itself.

### Topic cleanup

If the registry and the actual Telegram state drift apart, `doctor topics` can detect and optionally clean up orphan entries. See `docs/FEATURE-diagnostics.md` for the operator-facing diagnostic flow.

### Permission boundary

Topic cleanup that mutates the chat requires the bot to have `can_manage_topics`. If that permission is missing, the daemon will keep the topic in the report and skip the chat-side delete.

---

## Discord Notes

The docs mention Discord because the channel abstraction is not Telegram-only in spirit. The current implementation centers on Telegram, but the abstractions are designed so another platform can be added by implementing the same trait surface.

If a future Discord backend is added, the main expectations are the same:

- each agent needs a stable addressable conversation target
- messages must round-trip without losing thread identity
- registry state must be persisted so the daemon can recover after restart

---

## Common Workflows

### Operator sends a message

1. Open the agent's topic.
2. Type the message.
3. The daemon receives it, routes it to the agent, and mirrors the agent's response back to the same topic.

### Agent replies to operator

1. The agent emits a response through the channel layer.
2. The daemon posts or edits the corresponding Telegram message.
3. The operator sees the reply in context, with the topic history preserved.

### Diagnose drift

If topics seem to exist in Telegram but not in the daemon registry:

1. Run `agend-terminal doctor topics`.
2. Check whether entries are `live` or `orphan`.
3. Use `--cleanup` only after you confirm the action set.

---

## Example Config

A minimal working setup looks like this:

```yaml
channel:
  telegram:
    bot_token_env: AGEND_BOT_TOKEN
    group_id: -1001234567890
    user_allowlist: [123456789]
```

Then export the token:

```bash
export AGEND_BOT_TOKEN="123456:abcdef..."
```

If you want the operator to see the topic map in a group chat, make sure the group has Topics enabled and the bot has the permissions needed to create and manage topics.

---

## Troubleshooting

### Bot does not receive messages

Check the following in order:

1. The bot has joined the group and has read permission
2. The group has Topics enabled (Settings → Topics → on)
3. `user_allowlist` includes your Telegram user ID
4. Check the channel initialization messages in the daemon log

### Topic is not created automatically

1. Confirm the bot has `can_manage_topics` permission (set by the group admin)
2. Run `agend-terminal doctor topics` to check the status
3. Check the existing mapping in `topics.json`

### Duplicate messages

Adjust the dedup TTL:

```yaml
channel:
  dedup_ttl_secs: 10    # widen the window
```

If the problem persists, check whether multiple daemon instances are running at the same time.

---

## Source Pointers

- `src/channel/telegram.rs`: Telegram channel implementation
- `src/bootstrap/doctor_topics.rs`: topic classification and cleanup logic
- `src/cli.rs`: `doctor topics` CLI flow
- `src/main.rs`: subcommand routing and entry points
- `src/fleet.rs`: fleet and instance metadata, including topic fields

---

## Practical Advice

1. Treat the topic registry as state, not as a cache.
2. Always verify permissions before attempting cleanup.
3. Keep one topic per agent unless you have a specific reason to deviate.
4. Use `doctor topics` when the visible chat state and the daemon's state stop matching.
