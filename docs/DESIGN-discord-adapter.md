# Discord Adapter Design Document

**Feature branch:** `discord`
**Date:** 2026-04-21
**Status:** Draft

---

## 1. Overview

Add Discord as a second channel adapter in agend-terminal, alongside the existing Telegram adapter. Each agent instance maps to a Discord Text Channel inside an auto-managed Category, mirroring how Telegram maps instances to Forum Topics.

### Architecture Diagram

```
┌─────────────────────────────────────────────────────────┐
│                     fleet.yaml                          │
│  channel:                                               │
│    type: discord                                        │
│    bot_token_env: DISCORD_BOT_TOKEN                     │
│    guild_id: "123456789012345678"                       │
│    category_name: "AgEnD Agents"        (optional)      │
│    user_allowlist: ["111...", "222..."]  (optional)      │
└──────────────────────┬──────────────────────────────────┘
                       │
              ┌────────▼────────┐
              │  bootstrap::     │
              │  channel_init    │
              │  (new, generic)  │
              └────────┬────────┘
                       │
         ┌─────────────▼──────────────┐
         │      DiscordState          │
         │  ┌───────────────────────┐ │
         │  │ serenity::Client      │ │
         │  │ Http (for REST calls) │ │
         │  │ guild_id: GuildId     │ │
         │  │ category_id: ChannelId│ │
         │  │ channel_to_instance   │ │  HashMap<u64, String>
         │  │ instance_to_channel   │ │  HashMap<String, u64>
         │  │ submit_keys           │ │  HashMap<String, String>
         │  │ user_allowlist        │ │  Option<Vec<String>>
         │  │ home: PathBuf         │ │
         │  │ registry: Option<AR>  │ │
         │  └───────────────────────┘ │
         └─────────────┬─────────────┘
                       │
          ┌────────────┼────────────┐
          │            │            │
    ┌─────▼─────┐ ┌───▼───┐ ┌─────▼──────┐
    │ Inbound   │ │Outbound│ │ Channel    │
    │ EventHandler│ │ REST  │ │ Lifecycle  │
    │ (Gateway) │ │ calls  │ │ create/del │
    └───────────┘ └───────┘ └────────────┘
```

---

## 2. Data Structures

### 2.1 ChannelConfig — extend enum in `fleet.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ChannelConfig {
    #[serde(rename = "telegram")]
    Telegram { /* existing fields */ },

    #[serde(rename = "discord")]
    Discord {
        /// Env var name containing the Discord bot token.
        bot_token_env: String,
        /// Discord Guild (server) ID — stored as string to avoid
        /// YAML f64 precision loss on 64-bit snowflake IDs.
        guild_id: String,
        /// Category name to auto-create/find. Default: "AgEnD Agents".
        #[serde(default = "default_category_name")]
        category_name: String,
        /// Optional allowlist of Discord user IDs (snowflake strings).
        /// Semantics mirror Telegram: None = open, Some([]) = reject all.
        #[serde(default)]
        user_allowlist: Option<Vec<String>>,
    },
}

fn default_category_name() -> String {
    "AgEnD Agents".to_string()
}
```

**Snowflake ID handling:** Discord IDs are u64 but YAML/JSON float precision
truncates values > 2^53. `guild_id` and `user_allowlist` are stored as
`String` in the config. Internal runtime maps use `u64` after parsing.

### 2.2 InstanceConfig — extend `channel_id` field in `fleet.rs`

```rust
pub struct InstanceConfig {
    // ... existing fields ...
    pub topic_id: Option<i32>,          // Telegram (kept)
    pub channel_id: Option<String>,     // Discord — snowflake as string
}
```

### 2.3 DiscordState — new struct in `discord.rs`

```rust
pub struct DiscordState {
    pub http: Arc<serenity::http::Http>,
    pub guild_id: serenity::model::id::GuildId,
    pub category_id: serenity::model::id::ChannelId,
    pub channel_to_instance: HashMap<u64, String>,
    pub instance_to_channel: HashMap<String, u64>,
    pub home: PathBuf,
    pub submit_keys: HashMap<String, String>,
    pub user_allowlist: Option<Vec<u64>>,
    pub registry: Option<AgentRegistry>,
}
```

### 2.4 Channel Registry — `channels.json`

Persisted at `$AGEND_HOME/channels.json` (parallel to `topics.json`).

```json
{
  "1234567890123456": "alice",
  "1234567890123457": "bob"
}
```

Key = channel_id (string), Value = instance_name.

---

## 3. Core Flows

### 3.1 Initialization (`init_from_config`)

```
1. Read bot_token from env var (bot_token_env)
2. Parse guild_id string → u64 → GuildId
3. Create serenity Http client from token
4. Find or create the Category channel:
   a. GET /guilds/{guild_id}/channels
   b. Find existing category by name (category_name)
   c. If not found: POST create category channel (type=4)
   d. Store category_id
5. Load channel_registry (channels.json)
6. Clean up orphaned channels:
   - For each entry in registry not in fleet.yaml instances → delete channel
7. For each instance in fleet.yaml:
   a. If channel_id is set → verify channel exists, register mapping
   b. If instance is "general":
      - Use first existing text channel in category, or create one named "general"
   c. Else: create text channel under category, named after instance
   d. Write channel_id back to fleet.yaml
   e. Register in channels.json
8. Build DiscordState with all mappings
9. Start gateway polling (serenity Client) in dedicated thread
```

### 3.2 Inbound Message Handling (`EventHandler::message`)

```
1. Ignore bot messages (msg.author.bot)
2. Authz: check msg.author.id against user_allowlist
   - None → accept all (legacy open)
   - Some([]) → reject all
   - Some([...]) → must be in list
3. Resolve channel_id → instance_name via channel_to_instance map
   - Unknown channel → reload from fleet.yaml (same pattern as Telegram)
   - Still unknown → route to "general"
4. Check agent_wants_raw_keystrokes (same logic as Telegram):
   - If true → inject raw keystrokes via API INJECT call
   - If false → enqueue InboxMessage + notify_agent via PTY
5. Handle attachments:
   - Discord CDN URLs expire; download immediately to $AGEND_HOME/downloads/{instance}/
   - Append "[attachment: {filename}]" to message text
```

### 3.3 Outbound: send_reply / react / edit_message

```rust
// send_reply: split text at 2000 chars, send each chunk
pub fn try_discord_reply(instance_name: &str, text: &str) -> Result<(String, String)>
// Returns (message_id, channel_id) as strings

// react: add reaction emoji to message
pub fn try_discord_react(instance_name: &str, emoji: &str, message_id: Option<&str>) -> Result<()>

// edit_message: edit by message_id
pub fn try_discord_edit(instance_name: &str, message_id: &str, text: &str) -> Result<()>
```

**Message splitting** (2000 char limit):

```rust
fn split_message(text: &str, limit: usize) -> Vec<&str> {
    // Split on newline boundaries, falling back to char boundary at limit
}
```

### 3.4 Channel Lifecycle

```rust
// Create a text channel for a new instance under the AgEnD category
pub fn create_channel_for_instance(home: &Path, instance_name: &str) -> Option<String>

// Delete a text channel
pub fn delete_channel(home: &Path, channel_id: u64)
```

### 3.5 Channel Deletion Detection

Serenity `EventHandler::channel_delete` fires when a channel is removed.
Maps to the same cleanup path as Telegram's `forum_topic_closed`:

```
1. Look up channel_id in channel_to_instance
2. If found → cleanup_deleted_channel(home, instance_name, channel_id, state)
   - Remove from in-memory maps
   - Call API DELETE to stop the agent
   - Remove from fleet.yaml
   - Unregister from channels.json
```

---

## 4. fleet.yaml Configuration

```yaml
channel:
  type: discord
  bot_token_env: DISCORD_BOT_TOKEN
  guild_id: "123456789012345678"       # string — snowflake
  category_name: "AgEnD Agents"        # optional, default shown
  user_allowlist:                       # optional
    - "111111111111111111"
    - "222222222222222222"

defaults:
  backend: claude

instances:
  general:
    role: "General coordination"
    # channel_id auto-populated after first run
  alice:
    backend: kiro-cli
    role: "Developer"
    # channel_id: "987654321098765432"  # auto-populated
  bob:
    backend: claude
    role: "Reviewer"
```

---

## 5. Files to Modify / Create

| File | Action | Description |
|------|--------|-------------|
| `src/discord.rs` | **CREATE** | DiscordState, EventHandler, init_from_config, send/react/edit/download, channel lifecycle, ChannelAdapter impl |
| `src/fleet.rs` | MODIFY | Add `Discord` variant to `ChannelConfig`, add `channel_id: Option<String>` to `InstanceConfig`, add `channel_id` to `ResolvedInstance` |
| `src/channel.rs` | MODIFY | No structural changes needed — trait is already generic enough |
| `src/bootstrap/telegram_init.rs` | RENAME → `src/bootstrap/channel_init.rs` | Generalize to dispatch on `ChannelConfig` variant: Telegram → existing, Discord → new |
| `src/bootstrap/mod.rs` | MODIFY | Update module reference from `telegram_init` to `channel_init`, update `OwnedFleet.telegram` field to generic `channel: Option<Arc<Mutex<dyn ChannelAdapter>>>` or keep both |
| `src/ops.rs` | MODIFY | Add Discord dispatch in reply/react/edit/download — route based on active channel type |
| `src/mcp/handlers.rs` | MODIFY | Route channel tools (reply, react, edit, download) through channel-agnostic dispatch |
| `src/mcp/tools.rs` | MODIFY | Update tool descriptions from "Telegram" to "channel" (backward-compatible) |
| `src/app/telegram_hooks.rs` | RENAME → `src/app/channel_hooks.rs` | Generalize topic/channel create/delete hooks |
| `src/daemon/telegram.rs` | RENAME → `src/daemon/channel_notify.rs` | Generalize notify function to dispatch on channel type |
| `Cargo.toml` | MODIFY | Add `serenity` dependency |

---

## 6. Implementation Strategy

### Phase 1: Foundation (channel abstraction)

1. Add `channel_id: Option<String>` to `InstanceConfig` and `ResolvedInstance` in `fleet.rs`
2. Add `Discord` variant to `ChannelConfig` enum in `fleet.rs`
3. Add `serenity` to `Cargo.toml`:
   ```toml
   serenity = { version = "0.12", default-features = false, features = ["client", "gateway", "model", "rustls_backend", "cache"] }
   ```

### Phase 2: Core Discord module

4. Create `src/discord.rs` with:
   - `DiscordState` struct
   - Channel registry (channels.json) load/save/register/unregister
   - `init_from_config()` — category find/create, channel auto-create, orphan cleanup
   - `start_gateway()` — spawn serenity Client in dedicated thread with its own tokio runtime (same pattern as Telegram's `start_polling`)
   - Serenity `EventHandler` impl:
     - `message()` → inbound routing (authz → resolve → inbox/raw)
     - `channel_delete()` → cleanup
   - `split_message()` for 2000-char limit
   - Outbound: `try_discord_reply`, `try_discord_react`, `try_discord_edit`, `try_discord_download`
   - `ChannelAdapter` trait impl for `Arc<Mutex<DiscordState>>`

### Phase 3: Integration wiring

5. Generalize `bootstrap/telegram_init.rs` → `channel_init.rs`:
   - Match on `ChannelConfig` variant, call appropriate init
6. Update `bootstrap/mod.rs`:
   - Change `OwnedFleet.telegram` to a channel-agnostic type, or add `discord` field alongside
   - Recommended: keep `telegram: Option<Arc<Mutex<TelegramState>>>` and add `discord: Option<Arc<Mutex<DiscordState>>>` for simplicity — a full trait-object refactor can follow later
7. Update `ops.rs` channel functions to dispatch based on configured channel type
8. Update `mcp/handlers.rs` to route through ops (already partially done)
9. Generalize `app/telegram_hooks.rs` → `channel_hooks.rs`
10. Generalize `daemon/telegram.rs` → `channel_notify.rs`

### Phase 4: Polish

11. Update MCP tool descriptions (remove "Telegram" hardcoding)
12. Add `attach_registry()` for Discord (same pattern as Telegram)
13. Handle Discord-specific edge cases:
    - Rate limiting (serenity handles most, but bulk channel creation at init needs backoff)
    - CDN attachment download (URLs expire, must download immediately)
    - Nickname/display name resolution for `from:` field in InboxMessage

---

## 7. Key Design Decisions

### 7.1 Serenity crate over raw REST

Serenity provides Gateway (WebSocket) event handling, HTTP client, model types, and rate-limit management. This mirrors how `teloxide` serves the Telegram adapter. Use `rustls_backend` feature to match the project's TLS strategy (no OpenSSL).

### 7.2 Snowflake IDs as strings in config, u64 at runtime

YAML parses large integers as f64, losing precision beyond 2^53. Discord snowflakes are full u64. Solution: `String` in `fleet.yaml` / `ChannelConfig` / `channels.json`, parsed to `u64` at init time. This matches the TypeScript version's approach.

### 7.3 Dedicated thread + tokio runtime (same as Telegram)

Serenity's gateway client needs an async runtime. Spawn a dedicated thread with `tokio::runtime::Builder::new_current_thread()` — identical to the Telegram adapter pattern. This avoids polluting the main thread's event loop.

### 7.4 Channel-per-instance (not thread-per-instance)

Discord Text Channels map 1:1 to instances, grouped under a Category. This is the direct analog of Telegram Forum Topics. Discord Threads were considered but rejected: threads auto-archive, have different permission models, and don't appear as prominently in the channel list.

### 7.5 Incremental integration (no big trait-object refactor)

Keep `OwnedFleet` with explicit `telegram` and `discord` fields rather than `Box<dyn ChannelAdapter>`. Reason: the two adapters have different state types, and call sites (ops.rs, daemon notify, app hooks) need adapter-specific access for lifecycle management. A full abstraction can follow once both adapters are stable.

### 7.6 Message length: 2000 chars

Discord's message limit is 2000 characters. Split on newline boundaries when possible, hard-split at 2000 otherwise. Each chunk is sent as a separate message.

---

## 8. Discord Bot Permissions Required

The bot needs these Gateway Intents and permissions:

**Gateway Intents:**
- `GUILDS` — channel create/delete events
- `GUILD_MESSAGES` — inbound messages
- `MESSAGE_CONTENT` — read message text (privileged intent, must be enabled in Discord Developer Portal)

**Channel Permissions:**
- `MANAGE_CHANNELS` — create/delete text channels and categories
- `SEND_MESSAGES` — outbound replies
- `READ_MESSAGE_HISTORY` — context for edits
- `ADD_REACTIONS` — react to messages
- `ATTACH_FILES` — (future) send file attachments

---

## 9. Error Handling Patterns

Mirror Telegram's patterns:

| Scenario | Telegram | Discord |
|----------|----------|---------|
| Channel/topic deleted externally | `is_topic_deleted_error` + `cleanup_deleted_topic` | `channel_delete` event + `cleanup_deleted_channel` |
| Send failure to deleted channel | `handle_send_failure` checks error string | Check for `Unknown Channel` (10003) error code |
| Bot token missing | Skip init, log info | Same |
| Rate limit | teloxide handles | serenity handles (built-in ratelimiter) |
| Orphan cleanup at startup | Compare registry vs fleet.yaml | Same |

---

## 10. Testing Strategy

- Unit tests for `split_message`, channel registry CRUD, snowflake parsing, user allowlist logic (same pattern as `telegram.rs` tests)
- Integration test: mock serenity HTTP to verify init flow creates category + channels
- Manual test: deploy with a real Discord bot on a test server
