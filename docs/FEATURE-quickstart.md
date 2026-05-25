# Quickstart — Interactive First-Time Setup

## Motivation

AgEnD integrates multiple AI coding backends (Claude Code, Kiro CLI, Codex, OpenCode, Gemini, Agy), each with its own installation path, CLI arguments, and environment variables. The first question every new user faces is: "What do I have installed, and how do I get it running?"

`quickstart` answers that question. It auto-detects installed backends, walks you through Telegram notification setup, and generates a ready-to-use `fleet.yaml`. The whole process takes roughly 2-5 minutes.

```
agend-terminal quickstart
```

---

## Interactive Flow

### Step 1: Detect Installed Backends

On launch, quickstart scans your `$PATH` for known backend executables:

| Backend | Detected Command |
|---------|-----------------|
| Claude Code | `claude` |
| Kiro CLI | `kiro-cli` |
| Codex | `codex` |
| OpenCode | `opencode` |
| Gemini | `gemini` |
| Agy (Antigravity) | `agy` |

Detected backends are shown with their version number. If multiple are found, you choose a default. If only one is found, it is auto-selected.

If no backend is detected, quickstart lists all supported backends with installation hints and exits.

**Example output (multiple backends detected):**

```
Detected 2 AI coding backends:

  1. Claude Code (v1.2.3)
  2. Kiro CLI (v0.4.1)

Choose default backend [1]:
```

### Step 2: Telegram Setup

Telegram is AgEnD's primary notification channel. Quickstart guides you through bot configuration:

#### 2a. Obtain a Bot Token

Quickstart displays BotFather instructions:

1. Search for `@BotFather` in Telegram
2. Send `/newbot`
3. Follow the prompts to name your bot
4. Copy the generated token

After pasting the token, quickstart validates the format (`<8+ digits>:<30+ alphanumeric chars>`). Invalid formats prompt re-entry.

#### 2b. Verify the Bot

Once the format check passes, quickstart calls the Telegram `getMe` API to confirm the token is valid and retrieves the bot's username.

#### 2c. Detect Telegram Group

Quickstart asks you to add the bot to a Telegram supergroup, then send any message in that group. It uses 3-minute long-polling (`getUpdates`) to detect the group.

Detection requirements:
- Must be a **supergroup** (regular groups don't support topic mode)
- Bot must have **admin permissions** (required for managing topics)

On timeout, you can:
- **Retry**: Wait another 3 minutes
- **Skip**: Generate fleet.yaml now, configure Telegram later
- **Exit**: Abort quickstart

Maximum 3 retries before suggesting to skip.

#### 2d. Save the Token

After successful group detection, quickstart writes `AGEND_BOT_TOKEN` to `~/.env`.

Security measures:
- File permissions set to `0600` (owner read/write only, Unix)
- Checks whether `.gitignore` covers `.env` to prevent accidental commits

### Step 3: Generate fleet.yaml

Quickstart generates `fleet.yaml` under `$AGEND_HOME/`. If one already exists, it asks whether to overwrite.

Generated fleet.yaml contents:

```yaml
# Default backend
defaults:
  backend: claude  # or whichever backend you selected

# Telegram notification channel (if configured)
channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456789
  mode: topic
  user_allowlist:
    - 12345  # fill in your Telegram user ID

# Agent instances
instances:
  general:
    role: "General-purpose coding assistant"
    working_directory: ~/workspace
```

If Telegram was skipped, the channel block is preserved as comments for easy manual configuration later.

The `user_allowlist` field is always generated (even if empty) — this is the Sprint 21 fail-closed security design: Telegram users not on the allowlist cannot operate agents.

### Step 4: Next Steps

After completion, quickstart lists what to do next:

```
=== Before You Start ===

1. Edit user_allowlist in fleet.yaml with your Telegram user ID
2. Confirm the bot has admin permissions in the group
3. Confirm the group has been upgraded to a supergroup (for topic mode)

=== Launch ===

$ agend-terminal start        # Start the daemon
$ agend-terminal list          # Check agent status
$ agend-terminal attach general  # Connect to the agent terminal
```

---

## FAQ

### Q: I don't have Telegram. Can I skip it?

Yes. Choose "Skip" at the Telegram setup step. Quickstart generates a fleet.yaml without channel configuration. AgEnD still works normally — you just won't get Telegram notifications.

### Q: I want to use Discord instead of Telegram

Quickstart currently only supports automatic Telegram setup. For Discord, manually configure fleet.yaml:

```yaml
channel:
  type: discord
  bot_token_env: AGEND_DISCORD_TOKEN
  guild_id: "123456789"
```

### Q: Will quickstart overwrite my existing fleet.yaml?

When an existing fleet.yaml is detected, quickstart asks whether to overwrite. Choosing "No" keeps the existing file and exits quickstart.

### Q: Token format validation fails?

A valid Telegram bot token follows the format `<8+ digits>:<30+ alphanumeric characters, underscores, or hyphens>`. Copy the full token from BotFather — don't miss the numeric portion before the colon.

### Q: Why must the group be a supergroup?

AgEnD uses Telegram's topic (forum thread) feature to create separate conversation threads for each agent. Topics are only available in supergroups. To convert a regular group: Group Settings -> Enable "Topics".

---

## Technical Details

### Supported Backends

| Backend | Command | Default Args | Resume Support |
|---------|---------|-------------|---------------|
| Claude Code | `claude` | `--dangerously-skip-permissions` | `--continue` |
| Kiro CLI | `kiro-cli` | `--dangerously-skip-permissions` | `--resume` |
| Codex | `codex` | (version-dependent) | Built-in |
| OpenCode | `opencode` | (none) | `--continue` |
| Gemini | `gemini` | (none) | `--resume latest` |
| Agy | `agy` | (none) | `--continue` |

### File Locations

| File | Path | Description |
|------|------|-------------|
| fleet.yaml | `$AGEND_HOME/fleet.yaml` | Agent configuration |
| .env | `~/.env` | Bot token environment variable |

`$AGEND_HOME` defaults to `~/.agend-terminal`.

### Token Security

- The token is stored in fleet.yaml as an environment variable name (`AGEND_BOT_TOKEN`), not the plaintext value
- The actual token value only exists in `~/.env`
- `~/.env` permissions are `0600` (Unix)
- Quickstart traverses `.gitignore` files from the working directory up to the root to ensure `.env` is not tracked by git
