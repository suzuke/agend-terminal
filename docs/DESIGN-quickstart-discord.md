# Quickstart Discord Support

**Date:** 2026-04-22

---

## Problem

`quickstart.rs` hardcodes Telegram as the only channel option. Users who want Discord must manually edit fleet.yaml.

## Design

### New Flow

```
Quickstart
  │
  ▼
1. Detect backends (unchanged)
  │
  ▼
2. Channel selection:
   "Select channel: 1. Telegram  2. Discord  3. Skip"
  │
  ├─ Telegram → existing telegram_setup() (unchanged)
  ├─ Discord  → discord_setup()
  └─ Skip     → (empty token, no group_id/guild_id)
  │
  ▼
3. Save token to .env + generate fleet.yaml (channel-aware)
```

### `discord_setup()` Flow

```
  ── Discord Setup ──

  1. Go to https://discord.com/developers/applications
  2. Create application → Bot → copy token
  3. Enable MESSAGE CONTENT intent
  4. Invite bot to server with permissions: Manage Channels, Send Messages, Read Message History, Add Reactions

  Bot token (Enter to skip): ___

  Verifying bot... ✓ AgEnD Bot#1234

  Guild ID (right-click server → Copy Server ID): ___

  ✓ Verified guild: My Server
```

Steps:
1. Print Discord Developer Portal instructions
2. Prompt for bot token → verify via `GET https://discord.com/api/v10/users/@me` (Authorization: Bot {token})
3. Prompt for guild_id (string) → verify via `GET https://discord.com/api/v10/guilds/{guild_id}` → print server name
4. Return `(token, guild_id)`

### Changes to `generate_fleet_yaml()`

Add a `channel_type` parameter. Generate the appropriate channel section:

```yaml
# Discord
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_TOKEN
  guild_id: "123456789012345678"

# Telegram (existing, unchanged)
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456
  mode: topic
```

### Changes to `save_env_token()`

Generalize to accept env var name:

```
save_env_var(home, "AGEND_DISCORD_TOKEN", &token)
save_env_var(home, "AGEND_BOT_TOKEN", &token)     // Telegram
```

### Existing Config Detection

Extend the existing `.env` / `fleet.yaml` checks:
- Check for `AGEND_DISCORD_TOKEN` in `.env`
- Check for `channel.type: discord` + `guild_id` in fleet.yaml
- If found → "Use existing Discord config? (Y/n)"

## Files to Modify

| File | Change |
|------|--------|
| `src/quickstart.rs` | Add channel selection prompt, `discord_setup()`, generalize `save_env_token()` → `save_env_var()`, extend `generate_fleet_yaml()` with channel type, extend existing-config detection for Discord |

Single file change.

## Constraints

- Existing Telegram flow: zero behavior change when user selects Telegram
- No `#[cfg(feature = "discord")]` guard needed — quickstart only generates config, doesn't import serenity
- Discord token verification uses `reqwest` (already a dependency) — no new deps
- guild_id stored as string in fleet.yaml (snowflake precision)
