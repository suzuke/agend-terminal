use anyhow::Result;
use std::path::Path;
use tracing::{info, warn};

const AGEND_RULES: &str = r#"# AgEnD Terminal Communication

## How to respond to messages

When you see messages like `[user:NAME via telegram]` or `[from:INSTANCE]`, respond using Bash:

```bash
# Reply to the user who messaged you
agend-terminal reply "Your response here"

# Send a message to another agent instance
agend-terminal send TARGET "Message text"

# Check for pending messages
agend-terminal inbox
```

## Rules

- **Always use `agend-terminal reply`** to respond to `[user:... via telegram]` messages. Do NOT use the `reply` MCP tool.
- **Always use `agend-terminal send`** to communicate with other instances. Do NOT use `send_to_instance` MCP tool.
- Messages appear in your terminal as `[user:NAME via telegram] text` or `[from:INSTANCE] text`.
- For long messages, run `agend-terminal inbox` to see the full content.
- Keep replies concise and direct.
"#;

/// Generate agent instructions file for the given working directory.
/// For Claude Code: writes to `.claude/rules/agend.md`.
pub fn generate_for_claude(working_dir: &Path) -> Result<()> {
    let rules_dir = working_dir.join(".claude").join("rules");
    std::fs::create_dir_all(&rules_dir)?;
    let rules_path = rules_dir.join("agend.md");

    // Don't overwrite if already exists and has content
    if rules_path.exists() {
        let existing = std::fs::read_to_string(&rules_path)?;
        if existing.contains("agend-terminal reply") {
            info!("Instructions already exist: {}", rules_path.display());
            return Ok(());
        }
    }

    std::fs::write(&rules_path, AGEND_RULES)?;
    info!("Generated instructions: {}", rules_path.display());
    Ok(())
}

/// Generate instructions based on detected backend.
pub fn generate(working_dir: &Path, command: &str) {
    // Detect backend from command
    let is_claude = command.contains("claude");

    if is_claude {
        if let Err(e) = generate_for_claude(working_dir) {
            warn!("Failed to generate instructions: {e:#}");
        }
    }
    // Future: handle other backends (codex, gemini, etc.)
}
