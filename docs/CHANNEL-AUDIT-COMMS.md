# Channel-Agnostic Audit: Communication Layer

**Sprint 12 PR-AC** | Audited against main HEAD `904f67e` | 2026-04-25

## 1. Telegram Coupling Inventory

### Adapter boundary (`src/channel/`)
| File | Telegram refs | Role |
|------|--------------|------|
| `telegram.rs` | 203 | Primary adapter — all Telegram API calls, polling, message handling |
| `contract.rs` | 18 | Binding contract — `TelegramBinding` struct, topic resolution |
| `ux_event.rs` | 12 | UX event rendering — Telegram-specific formatting |
| `binding.rs` | 8 | Binding ref — `TelegramBindingRef` inner type |
| `mod.rs` | 7 | Channel trait + Telegram-specific `attach_registry` |
| `sink_registry.rs` | 6 | UX sink — Telegram-specific sink impl |
| `event.rs` | 5 | Event types — `ChannelEvent` variants |
| `caps.rs` | 5 | Capability matrix — Telegram-specific caps |

### Leaks outside adapter boundary
| File | Refs | Nature of leak |
|------|------|---------------|
| `fleet.rs` | 28 | `ChannelConfig::Telegram` variant in fleet.yaml schema — **structural coupling** |
| `mcp/handlers.rs` | 20 | `try_telegram_react`, `try_telegram_edit` — MCP tools directly call Telegram adapter |
| `quickstart.rs` | 18 | Telegram setup wizard — hardcoded Telegram onboarding |
| `app/overlay.rs` | 17 | Telegram topic picker overlay — UI coupled to Telegram concepts |
| `bootstrap/mod.rs` | 16 | `telegram_init` — bootstrap hardcodes Telegram channel creation |
| `app/telegram_hooks.rs` | 15 | Dedicated Telegram hook file — should be adapter-internal |
| `app/mod.rs` | 15 | Telegram state threading through app lifecycle |
| `render.rs` | 11 | Telegram-specific rendering (topic names, status) |
| `daemon/mod.rs` | 11 | Telegram channel init + state passing |
| `inbox.rs` | 5 | `NotifySource::Telegram` + Telegram-specific reply hint formatting |
| `api/handlers/instance.rs` | 3 | API spawn calls `crate::channel::telegram::create_topic_for_instance` |
| `api/handlers/team.rs` | 3 | Team/deploy path creates Telegram topic |
| `bootstrap/telegram_init.rs` | 39 | Dedicated Telegram bootstrap wrapper (separate from bootstrap/mod.rs) |
| `daemon/supervisor.rs` | 5 | Direct Telegram notification plumbing for stall notice |
| `mcp/tools.rs` | 1 | Public tool description says "Reply to the user via Telegram" |

## 2. Abstraction Surface

### Channel trait (`src/channel/mod.rs`)
Already exists: `pub trait Channel: Send + Sync` with methods:
- `kind()`, `caps()`, `poll_event()`, `send()`, `edit()`, `delete()`, `react()`
- **Good**: trait is channel-agnostic in shape
- **Gap**: `attach_registry` is on the trait but semantically Telegram-specific (agent registry for topic routing)

### OutMsg (`src/channel/event.rs`)
```rust
pub struct OutMsg { pub text: String }
// TODO(T1b+): buttons, attachments, reply_to, parse mode override.
```
- **No attachment field** — outbound media impossible without struct change
- **No reply_to** — threading not supported outbound
- **No parse_mode** — markdown/HTML formatting not controllable

### InboxMessage (`src/inbox.rs`)
- ⚠️ **Telegram metadata leaks via magic strings and formatting**, even though struct fields are generic:
  - `telegram.rs:436-449` — InboxMessage enqueued with `kind: Some("telegram")` magic string
  - `telegram.rs:406-409` — `pending_pickup_ids` entry carries `"kind": "telegram"` + Telegram `msg_id`
  - `inbox.rs:124-148` — `NotifySource::Telegram` variant produces Telegram-specific reply hint ("Reply using the reply tool")
- Channel-agnostic design must address: `kind` field carries channel identity as untyped string; reply hints are channel-specific formatting baked into inbox delivery

## 3. Inbound Media Gaps

| Media type | Handled? | Location |
|-----------|---------|----------|
| Text | ✅ | `telegram.rs` `handle_message()` |
| Photo | ❌ | `msg.photo()` → early return (PR #54 proposes fix) |
| Document | ❌ | `msg.document()` → early return (PR #54 proposes fix) |
| Voice | ❌ | Not handled, silently dropped |
| Video | ❌ | Not handled, silently dropped |
| Sticker | ❌ | Not handled, silently dropped |
| Location | ❌ | Not handled, silently dropped |

## 4. Outbound Media Gaps

- `OutMsg` has no `attachment` field — cannot send files/images/audio
- No `send_photo` / `send_document` / `send_voice` on Channel trait
- Telegram adapter has raw `bot.send_message()` but no media send wrappers

## 5. PR #53 (Discord Adapter) Prior Art

**Design axis**: Feature-gated (`discord` feature flag) + `serenity` crate dependency.

**Key structures**:
- Adds `ChannelConfig::Discord` variant to `fleet.rs` (parallel to `Telegram`)
- Implements `Channel` trait for Discord adapter
- Uses `serenity::Client` for Discord API

**Conflict surface with main**:
- `fleet.rs` `ChannelConfig` enum — additive (new variant), low conflict
- `Cargo.toml` — new dependency + feature flag
- `channel/mod.rs` — new adapter registration
- `bootstrap/` — Discord init parallel to Telegram init

## 6. PR #54 (Photo/Document Inbound) Prior Art

**Design axis**: Extends `handle_message()` in `telegram.rs` to extract photo/document file_id.

**Actual implementation** (from `gh pr diff 54`):
- Modifies `handle_message()` to extract `msg.photo()` largest PhotoSize file_id and `msg.document()` file_id
- Adds `try_download_attachment_to(...)` helper — downloads via Telegram `get_file` API to local path
- Appends `[attachment: {file_id} -> {local_path}]` string to message text
- **Does NOT modify `InboxMessage` struct** — no new fields, no separate attachment metadata
- Caption becomes the text; attachment info is inline text, not structured data

**Conflict surface with main**:
- `telegram.rs` `handle_message()` — direct modification of existing function
- No `InboxMessage` struct change — low conflict
- Low conflict with PR #53 (different adapter vs same adapter extension)
- `OutMsg` gap remains unaddressed

## 7. Summary

**Telegram boundary**: Well-defined via `Channel` trait, but 15+ files outside `src/channel/` have direct Telegram references. The biggest leaks are in `fleet.rs` (config schema), `mcp/handlers.rs` (react/edit tools), `bootstrap/` (init), `api/handlers/` (topic creation), and `inbox.rs` (channel-specific metadata via magic strings).

**Outbound media**: Completely missing — `OutMsg` is text-only with a TODO comment.

**Inbound media**: Only text handled; photo/document/voice/video/sticker all silently dropped. PR #54 addresses photo+document via text-append (no struct change).

**PR #53/#54 compatibility**: Both are additive and low-conflict with each other. PR #53 adds a parallel adapter; PR #54 extends Telegram inbound with file download + text append. Neither addresses the `OutMsg` gap or `InboxMessage` metadata coupling.
