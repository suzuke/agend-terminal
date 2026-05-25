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

The core code only talks to the trait. It never calls Telegram APIs directly. If we add Discord or another backend later, it only needs to implement this trait.

### Binding

A binding links an agent to a channel-specific address such as a Telegram topic ID. The core code treats the binding as opaque.

```text
agent "dev" ← binding → Telegram topic #42
agent "reviewer" ← binding → Telegram topic #43
```

### Topics and Registry

The daemon keeps a registry of topics so it can reconcile what is live in Telegram with what is known on disk. That registry is the source of truth for topic routing, while the topic itself is the operator-visible conversation surface.

### Inbound vs Outbound

Channels handles two directions:

- **Inbound**: operator or external events flow into the daemon, then into the agent.
- **Outbound**: the agent's reply is mirrored back to the same channel surface.

The code path is symmetric enough that debugging either side usually starts from the same topic entry and binding record.

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

### The agent never receives messages

Check the following in order:

- `fleet.yaml` has a valid `channel.telegram` block
- the bot token env var is exported
- the group ID is correct and the bot is inside the group
- the topic mapping exists in `topics.json`
- the daemon is actually running and polling

### The topic exists but replies do not appear

This usually means one of two things:

- the binding points at the wrong topic ID
- the daemon can receive but cannot post back because of a permission or API failure

### `doctor topics` reports orphans

That means the registry and the live chat state have diverged. Usually the right fix is to run cleanup with the correct permissions, not to hand-edit the registry blindly.

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
