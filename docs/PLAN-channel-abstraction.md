# PLAN: Channel abstraction for multi-platform messaging

**Branch:** `feat/channel-abstraction` (to be created when implementation starts)
**Date:** 2026-04-20
**Related:** `docs/PLAN-telegram-topic-delete-sync.md` (can be folded into this plan's Stage A if implemented together)

---

## 1. Background

Today `src/telegram.rs` owns every messaging concern: inbound polling,
outbound send, topic lifecycle, rate handling, and per-instance binding state.
The `src/channel.rs` trait exists but is a thin facade — most core code still
reaches into Telegram-specific types (`TelegramState`, `topic_id`,
`forum_topic_closed`).

We expect to add Discord as the second backend, and plausibly Slack / Matrix /
others later. Keeping Telegram-specific knowledge scattered through the core
makes every new channel a rewrite rather than a plugin.

## 2. Cross-platform reality check

Survey of the feature matrix that the abstraction must absorb:

| Dimension | Telegram | Discord | Slack | Matrix | LINE | IRC |
|---|---|---|---|---|---|---|
| Transport | Long-poll / webhook | WS Gateway | Events API / Socket | WS / REST | Webhook | WS |
| Thread model | forum topic | thread in channel | thread on message | `M.thread` | none | none |
| Deletion event | **no** | `channelDelete` | `channel_deleted` | tombstone | — | — |
| Interactive | inline keyboard | components | block kit | — | quick reply | — |
| Rate limit | group / bot quota | REST bucket | per-method | homeserver | push quota | informal |

Once platform-specific terms are stripped, the **irreducible semantics** are:

1. Inbound events (someone said / clicked / revoked a binding somewhere).
2. Outbound actions (send / edit / delete a message at a binding).
3. Binding lifecycle (pair an agent to a "place"; unpair).

Platform-specific features (markdown dialect, button payload shape, rate
budget) are **declared via capabilities**, not hardcoded into the trait.

## 3. Design

### 3.1 Core trait

```rust
trait Channel: Send + Sync {
    fn kind(&self) -> &'static str;              // "telegram" / "discord"
    fn caps(&self) -> &ChannelCapabilities;

    fn events(&self) -> EventStream;             // inbound: ChannelEvent stream
    fn send(&self, b: &BindingRef, m: OutMsg) -> Result<MsgRef>;
    fn edit(&self, m: &MsgRef, payload: OutMsg) -> Result<()>;
    fn delete(&self, m: &MsgRef) -> Result<()>;

    fn create_binding(&self, name: &str, opts: BindingOpts) -> Result<BindingRef>;
    fn remove_binding(&self, b: &BindingRef) -> Result<()>;
}
```

### 3.2 `BindingRef` is opaque

A `BindingRef` wraps whatever the platform needs to address a "place":

- Telegram: `{ chat_id, topic_id }`
- Discord: `{ guild_id, channel_id }`
- Slack: `{ workspace, channel, thread_ts }`

Core code **never inspects the inside** — it hands the ref back to the right
channel. Each agent instance holds `Vec<BindingRef>` (supports multi-channel
binding; see §3.6).

### 3.3 Normalised inbound events

```rust
enum ChannelEvent {
    MessageIn {
        binding: BindingRef,
        from: User,
        payload: MsgPayload,
        ts: DateTime<Utc>,
    },
    ButtonClick { binding: BindingRef, from: User, data: String },
    BindingRevoked { binding: BindingRef, reason: RevokeReason },
    Connected { kind: String, who: String },
    Disconnected { kind: String, reason: Option<String> },
}

enum RevokeReason { Closed, Deleted, Archived, Unknown }
```

`BindingRevoked` is the key unification: Telegram's `forum_topic_closed`,
Discord's `channelDelete`, Slack's `channel_deleted` all emit this same event
with an appropriate `reason`. Core handlers stop caring which platform
triggered cleanup.

### 3.4 Capabilities drive fallback behaviour

```rust
struct ChannelCapabilities {
    emits_deletion_events: bool,     // TG=false, Discord=true
    threads: bool,
    buttons: bool,
    attachments: bool,
    markdown: MarkdownDialect,       // MarkdownV2 / DiscordMd / SlackMrkdwn / None
    max_msg_bytes: usize,
    rate_budget: RateBudget,
}
```

Core queries caps when deciding:

- `emits_deletion_events == false` → register error-driven cleanup fallback
  (the mechanism from `PLAN-telegram-topic-delete-sync.md`).
- `markdown` → format outbound message with the right dialect before calling
  `send`.
- `max_msg_bytes` → split long messages into chunks.

This replaces platform if-else branches in core with capability predicates.

### 3.5 Rate budget abstraction

Every channel declares its `RateBudget` (e.g. `{ per_minute: 20, per_second: 30 }`).
Core wraps outbound calls in a shared token-bucket layer. Benefits:

- Per-channel limits don't bleed into each other.
- Future active-probe features (e.g. `editForumTopic` no-op) become gated by
  the same budget, making cost explicit.
- Rate-limit errors from the server can top up `retry_after` seconds into the
  same bucket — unified handling.

### 3.6 Multi-channel binding in config

```yaml
channels:
  tg-main:
    type: telegram
    bot_token_env: TG_TOKEN
    group_id: -100123456
    user_allowlist: [111, 222]
  discord-ops:
    type: discord
    bot_token_env: DC_TOKEN
    guild_id: 987654
    category_name: agents

instances:
  alpha:
    backend: claude
    channels: [tg-main]                # single binding, today's default
  beta:
    backend: codex
    channels: [tg-main, discord-ops]   # dual binding, events merged
```

`channel:` (singular, legacy) still parses as a single unnamed channel for
backward compat; new `channels:` (plural, named) is the forward path.

### 3.7 Dispatch layer

```
┌───────────────────┐
│  ChannelRegistry  │   holds Box<dyn Channel> keyed by channel name
└─────┬─────────────┘
      │ merges events() streams
      ▼
┌─────────────────┐        ┌──────────────────┐
│  InboundRouter  │──────▶│  AgentDispatcher │  (same code as today)
└─────────────────┘        └──────────────────┘

Outbound:
  core → InstanceSend { channels, payload }
       → registry.get(ch).send(binding, payload)  per channel in the instance
```

The merge point is a `crossbeam::channel::Receiver<ChannelEvent>` or
`tokio::mpsc`, whichever matches the existing runtime. Inbound routing looks
up the agent via `BindingRef → instance_name` (registry per-channel map).

## 4. Distribution strategy (Rust reality)

| Mode | When | Cost | Ceiling |
|---|---|---|---|
| `cargo feature` gates | ≤3 channels | low | core rebuild per channel |
| Sidecar process + IPC | 5+ channels / community plugins | medium (IPC infra already exists) | near-unlimited |
| WASM plugin | far future | high | strongest isolation |

**Near-term:** cargo features (`--features "discord slack"`). Simple, no
runtime plugin infrastructure, opt-in build size.

**Long-term:** sidecar over the existing daemon IPC (loopback + cookie). Each
channel runs as a separate process, exchanges `ChannelEvent` JSON with the
daemon. This is a natural fit because we already have the IPC machinery —
effectively AgEnD's npm-plugin model but language-neutral.

WASM is noted for completeness but not on the roadmap.

## 5. Staged rollout

### Stage A — Decouple Telegram (this repo, no new features)

**Goal:** zero-behaviour-change refactor that isolates Telegram behind the new
trait. Prerequisite for any downstream channel.

- Define `src/channel/{mod,event,caps,binding}.rs` with the trait +
  `ChannelEvent` + `BindingRef` + `ChannelCapabilities`.
- Move `src/telegram.rs` → `src/channel/telegram.rs`, implement `Channel` on a
  wrapper type. Translate internal `forum_topic_closed` → `BindingRevoked`,
  `handle_message` → `MessageIn` etc.
- Route core code through `ChannelRegistry::default_channel()` (single-channel
  legacy path) instead of `TelegramState` directly.
- **Can absorb the PLAN-telegram-topic-delete-sync work:** the error-driven
  cleanup becomes the first capability-gated fallback
  (`caps().emits_deletion_events == false`). If scheduled together, write the
  helper at the trait layer, not Telegram-specific.

Exit criteria: all existing Telegram tests pass unchanged; `src/telegram.rs`
does not exist at its current path; nothing outside `src/channel/telegram.rs`
references `teloxide` types.

### Stage B — Discord (first real pressure test)

**Goal:** add Discord via the new abstraction. If the trait cracks under this
load, **stop and redesign before continuing** — better than paving over bad
abstraction with a second impl.

- `src/channel/discord.rs` behind `--features discord`, backed by
  [`serenity`](https://github.com/serenity-rs/serenity).
- Map `channelDelete` → `BindingRevoked { reason: Deleted }`,
  `messageCreate` → `MessageIn`, etc.
- `caps().emits_deletion_events = true` → core automatically skips the
  error-driven fallback for Discord bindings.
- Config schema supports `type: discord` per §3.6.
- Integration test covering bidirectional flow (send, receive, binding revoke
  via channel delete).

Exit criteria: Discord agent can be spawned, receives commands, emits replies,
pane auto-closes on `channelDelete`. Telegram behaviour unchanged.

**Abort signal:** if the `Channel` trait requires >2 breaking signature
changes during Stage B, stop. The sample is large enough to redesign.

### Stage C — Third channel (probably Slack) — validation only

Not a feature milestone; it's a validation step. If Stage B shipped with trait
stable, adding Slack should mostly be an `impl Channel` exercise. If Slack
forces another breaking change, freeze on two channels and revisit the trait
before accepting more.

### Stage D — Sidecar plugins (demand-driven, not scheduled)

Only pursue when there's concrete user demand for a channel we don't want to
ship in-tree (Matrix, Lark, Mattermost, IRC, ...). Reuse the daemon IPC layer
as the plugin protocol. Defer all design work until then — over-engineering
this now is the main trap.

## 6. Non-goals of this plan

- **Feature parity across channels.** If Discord has components and Telegram
  doesn't, core must degrade gracefully via capability check — we do not
  polyfill.
- **Unified markdown.** We do not rewrite agent output to a common markdown
  dialect; each channel formats its own on send. Expecting a uniform dialect
  across Telegram MarkdownV2 and Discord md is a losing battle.
- **Authoritative cross-channel user identity.** A Telegram user and a Discord
  user are distinct identities. Core does not map them; access control stays
  per-channel.
- **Replacing the existing daemon/agent pipeline.** Channels are edges; the
  daemon/agent core is untouched.

## 7. Migration & backward compatibility

- Legacy `channel:` (singular) config continues to parse; it becomes a named
  channel `default` internally. Existing `fleet.yaml` files work unchanged.
- Legacy per-instance `topic_id` field: keep in `fleet.yaml` for Telegram
  serialization; inside core it is hidden behind `BindingRef`.
- Tests that mock Telegram directly (e.g. `tests/telegram-api-root.test.rs`)
  keep working — the mock implements `Channel` now instead of raw HTTP.
- Existing `TelegramState` / `lock_state` API stays available as a
  `src/channel/telegram.rs`-internal detail; external callers get the trait.

## 8. Risk log

| Risk | Mitigation |
|---|---|
| Trait over-fits to Telegram's synchronous polling model | Stage B Discord forces async / gateway-style usage; address in trait v2 |
| Tokio vs crossbeam mismatch (daemon is largely sync) | Wrap async event sources in a dedicated runtime, bridge via mpsc to the sync core loop |
| `BindingRef` enum bloat as channels are added | Use `Box<dyn Any + Send + Sync>` internally or serde-tagged JSON for maximum decoupling; accept minor runtime cost |
| Rate budget drift (server-side limits change) | Keep `RateBudget` declared in code but allow runtime override via `fleet.yaml` |
| Feature-flag combinatorial CI | Ship a `ci-all-channels` feature alias and build only that matrix entry, not every subset |

## 9. Verification

### 9.1 Contract tests (trait-level)

Write a shared `tests/channel_contract.rs` that takes any `Channel` impl and
exercises the core contract: send round-trip, binding lifecycle, event stream
emits `Connected` + `Disconnected`, `BindingRevoked` delivery. Each channel's
test file runs the contract tests against a mock server for that platform.

### 9.2 Stage A exit test

Full Telegram integration suite passes unchanged. `cargo run` with a
Telegram-configured fleet behaves identically to before refactor (same topic
creation, same pane lifecycle, same inbox routing).

### 9.3 Stage B exit test

Dual-channel fleet (`channels: [tg-main, discord-ops]`) spawns one agent,
receives commands on both channels, `channelDelete` on Discord closes the pane
without affecting the Telegram binding, `forum_topic_closed` on Telegram
closes the pane without affecting Discord.

## 10. Implementation checklist (Stages A + B)

### Stage A
- [ ] Create `src/channel/` module with trait, event, caps, binding types.
- [ ] Move Telegram into `src/channel/telegram.rs` behind the trait.
- [ ] Register default channel in bootstrap (single-channel legacy path).
- [ ] Route core code through `ChannelRegistry`.
- [ ] Update `fleet.yaml` parser to accept `channel:` (singular) and
      `channels:` (plural) forms.
- [ ] Contract test harness.
- [ ] Migrate existing Telegram tests to the new module path.
- [ ] Land error-driven cleanup fallback at the trait layer (fold in from
      `PLAN-telegram-topic-delete-sync.md` if co-scheduled).

### Stage B
- [ ] Add `serenity` dep behind `--features discord`.
- [ ] `src/channel/discord.rs` with `Channel` impl + capability declaration.
- [ ] Config parsing for `type: discord`.
- [ ] Gateway event → `ChannelEvent` mapping (messageCreate, channelDelete,
      interactionCreate).
- [ ] Contract tests for Discord.
- [ ] Manual verification: dual-channel fleet spawns and reacts to both.
- [ ] Docs: README Channel section, fleet.yaml example updated.

## 11. Success criteria

- Telegram behaviour unchanged after Stage A (no regressions in test suite or
  manual flows).
- Discord agent in Stage B achieves the same lifecycle as a Telegram agent
  (spawn, binding create, send, receive, binding revoke, cleanup) without any
  special-case branches in core code.
- Adding a third channel in Stage C requires only a new `impl Channel` and a
  config schema extension — no core refactor.
