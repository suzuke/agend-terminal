# AgEnD Terminal

Rust-based Agent Process Manager — replaces tmux for managing AI coding agents.

Direct PTY ownership eliminates `send-keys` race conditions and enables atomic writes, output capture, and virtual terminal state tracking.

## Quick Start

```bash
# Build
cargo build --release

# Configure
mkdir -p ~/.agend-terminal
cp tests/test-fleet.yaml ~/.agend-terminal/fleet.yaml
# Edit fleet.yaml with your instances

# Start (daemon + fleet)
agend-terminal start

# Or start daemon only
agend-terminal daemon
agend-terminal fleet start
```

## Commands

### Session Management
```bash
agend-terminal start                    # Daemon + fleet start
agend-terminal daemon                   # Bare daemon (no fleet)
agend-terminal list                     # List sessions
agend-terminal attach <id>              # Attach to session (Ctrl+B d to detach)
agend-terminal inject <id> <text>       # Send raw input
agend-terminal kill <id>                # Kill session
```

### Fleet Management
```bash
agend-terminal fleet start [config.yaml] [name...]
agend-terminal fleet stop [name...]
agend-terminal create-instance --name N --command C [options]
```

### Agent Communication (from inside a session)
```bash
agend-terminal reply "response text"        # Reply to user
agend-terminal send <target> "message"      # Message another instance
agend-terminal inbox                        # Check pending messages
```

## Configuration

### fleet.yaml
```yaml
defaults:
  command: claude
  args: ["--dangerously-skip-permissions"]
  ready_pattern: "All tools are now trusted"

channel:
  type: telegram
  bot_token_env: AGEND_BOT_TOKEN
  group_id: -100123456789

instances:
  general:
    role: "Fleet coordinator"
    working_directory: ~/project
    topic_id: 1
```

### Environment
- `AGEND_TERMINAL_HOME` — Data directory (default: `~/.agend-terminal`)
- `.env` file in home directory is auto-loaded on startup

## Architecture

See [docs/architecture.md](docs/architecture.md) for the full design.

## License

MIT
