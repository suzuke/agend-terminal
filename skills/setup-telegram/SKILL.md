---
name: setup-telegram
description: Interactive guide to configure a Telegram channel for AgEnD — creates bot, detects group, writes fleet.yaml
---

[繁體中文](SKILL.zh-TW.md)

# /setup-telegram — Telegram Channel Setup

Guide the user through setting up a Telegram bot channel for AgEnD. Use `AskUserQuestion` for choices, `Bash` (curl) for API verification, and `Read`/`Edit`/`Write` for config files.

## Prerequisites

Before starting, locate the AgEnD home directory:

```bash
echo "${AGEND_HOME:-$HOME/.agend-terminal}"
```

Store this as `AGEND_HOME` for all subsequent steps.

## Step 1: Create a Bot via BotFather

Tell the user:

> 1. Open Telegram and talk to **@BotFather**
> 2. Send `/newbot` and follow the instructions to name your bot
> 3. Copy the bot token (looks like `123456789:ABCdef...`)

Ask for the token using `AskUserQuestion`:
- Question: "Paste your bot token from BotFather"
- Options: provide a "Skip — configure later" option

If skipped, tell the user they can run `/setup-telegram` again later and stop here.

## Step 2: Validate Token Format

Check the token matches the pattern `<digits>:<35+ alphanumeric chars>`:

```bash
echo "$TOKEN" | grep -qE '^[0-9]{8,}:[A-Za-z0-9_-]{30,}$'
```

If invalid, warn the user and ask whether to re-enter, continue anyway, or skip.

## Step 3: Verify Bot with Telegram API

```bash
curl -s "https://api.telegram.org/bot${TOKEN}/getMe"
```

Check that `result.is_bot` is `true`. Print the bot username on success. On failure, offer re-enter or skip.

## Step 4: Group Setup

Tell the user:

> 1. Create a Telegram **supergroup** (or use an existing one)
> 2. Enable **Topics** in group settings (Group → Edit → Topics)
> 3. Add the bot to the group **as admin**
> 4. Send any message in the group

Then poll for the group using `getUpdates`:

```bash
curl -s "https://api.telegram.org/bot${TOKEN}/getUpdates?timeout=30&allowed_updates=[\"message\"]"
```

Parse the response to find a supergroup chat (type == "supergroup"). Extract `chat.id` and `chat.title`. Retry up to 6 times (total ~3 minutes). If no group detected, ask the user to enter the group_id manually or skip.

## Step 5: Verify Bot is Admin

```bash
curl -s "https://api.telegram.org/bot${TOKEN}/getChatMember?chat_id=${GROUP_ID}&user_id=${BOT_ID}"
```

Check that `result.status` is `"administrator"` or `"creator"`. If not, warn the user:

> The bot must be a group admin for topic mode to work. Go to group settings and promote the bot to admin.

This is a warning, not a blocker — continue regardless.

## Step 6: User Allowlist

Ask the user for their Telegram user ID(s):

> Send a message to **@userinfobot** on Telegram to get your user ID (a number like `123456789`).

Collect one or more user IDs. These go into `user_allowlist` in fleet.yaml.

## Step 7: Save Token Securely

Save the token to `$AGEND_HOME/.env`:

```bash
# Read existing .env, replace or append AGEND_BOT_TOKEN
```

Rules:
- If `AGEND_BOT_TOKEN` already exists in `.env`, ask before overwriting (default: keep existing)
- After writing, set permissions: `chmod 600 "$AGEND_HOME/.env"`
- Check if `.gitignore` covers `.env` — if not, warn the user

**NEVER write the token value directly into fleet.yaml or any YAML config.** Always use `bot_token_env: AGEND_BOT_TOKEN` which references the environment variable.

## Step 8: Update fleet.yaml

Read the existing `$AGEND_HOME/fleet.yaml`. If a `channel:` section already exists, ask before overwriting (default: keep existing, back up to `fleet.yaml.bak`).

Add or update the channel section:

```yaml
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: <detected or entered group_id>
  mode: topic
  user_allowlist: [<user_ids>]
```

Use `Edit` to modify the existing file if possible, or `Write` if creating from scratch.

## Step 9: Final Checklist

Print a summary:

> **Setup complete!**
> - Bot: @<bot_username>
> - Group: <group_title> (<group_id>)
> - Token: stored in `$AGEND_HOME/.env` (env var: `AGEND_BOT_TOKEN`)
> - Config: `$AGEND_HOME/fleet.yaml` updated
>
> **Next steps:**
> 1. Restart the daemon: `agend restart`
> 2. Send a message in the Telegram group to verify delivery

## Security Guardrails

These rules are mandatory and must not be bypassed:

1. **Token as env var reference only** — fleet.yaml must use `bot_token_env: AGEND_BOT_TOKEN`, never inline the token string
2. **chmod 600** — `.env` file must be owner-read/write only
3. **gitignore check** — warn if `.env` is not covered by `.gitignore`
4. **Token format validation** — verify format before making API calls
5. **No token in output** — when displaying the token back to the user, mask it: show first 4 and last 4 characters only (e.g., `1234...wxyz`)
